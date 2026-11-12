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
        message_count: Some(msgs),
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
fn compact_pipe_is_an_operator_but_quoted_pipe_is_literal() {
    for source in [
        "harness:claude|harness:codex",
        "harness:claude |harness:codex",
        "(harness:claude)|harness:codex",
    ] {
        assert_eq!(q(source).expr, q("harness:claude OR harness:codex").expr, "{source}");
    }
    let r = refx("alpha", Harness::Claude, None, Some("a|b"), 1);
    assert!(q("title=\"a|b\"").prefilter(&r));
    assert!(q("\"a|b\"").prefilter(&r));
    let url = refx("alpha", Harness::Claude, None, Some("https://host/a|b"), 1);
    assert!(q("title=\"https://host/a|b\"").prefilter(&url));
    assert!(q("\"https://host/a|b\"").prefilter(&url));
    assert_eq!(q("title:\"a|b\"|title:c").expr, q("title:\"a|b\" OR title:c").expr);
    // Regex matching was removed: quoting preserves the needle as literal text, including pipes.
    assert!(q("title:\"a|b\"").prefilter(&r));
    assert!(SessionQuery::parse("title~\"a|b\"")
        .unwrap_err()
        .contains("regex operator was removed"));
    for source in ["|harness:claude", "harness:claude|", "harness:claude||harness:codex"] {
        assert!(SessionQuery::parse(source).is_err(), "{source}");
    }
}

#[test]
fn catalog_string_equality_differs_from_contains() {
    let r = refx(
        "Alpha-long",
        Harness::Claude,
        Some("/Repo/child"),
        Some("Exact Title"),
        1,
    );
    for field in ["id", "cwd", "title"] {
        let (part, whole) = match field {
            "id" => ("alpha", "ALPHA-LONG"),
            "cwd" => ("/repo", "/REPO/CHILD"),
            _ => ("title", "exact title"),
        };
        assert!(q(&format!("{field}:\"{part}\"")).prefilter(&r));
        assert!(!q(&format!("{field}=\"{part}\"")).prefilter(&r));
        assert!(q(&format!("{field}=\"{whole}\"")).prefilter(&r));
    }
    assert!(!q("cwd=/repo").prefilter(&refx("alpha", Harness::Claude, None, None, 1)));
}

#[test]
fn invalid_queries_point_to_current_schema_command() {
    for source in [
        "title:\"unterminated",
        "(title:x",
        "harness:no-such-harness",
        "has:no-such-flag",
        "msgs:no-number",
        "updated:no-date",
        "title~pattern",
        "thread:old",
        "text=word",
        "before>=2026-01-01",
    ] {
        let error = SessionQuery::parse(source).unwrap_err();
        assert!(error.contains("see `cv schema`"), "{source}: {error}");
        assert!(!error.contains("cv query"), "{source}: {error}");
    }
}

#[test]
fn ranges_reject_ignored_operators_and_include_date_upper_days() {
    for source in [
        "msgs>1..3",
        "msgs<=1..3",
        "msgs:3..1",
        "updated>2026-01-01..2026-01-02",
        "updated:2026-01-02..2026-01-01",
        "harness:",
        "harness:,",
        "has:",
        "has:,",
        "NOT has:",
    ] {
        assert!(SessionQuery::parse(source).is_err(), "{source}");
    }
    for source in ["msgs:1..3", "msgs=1..3"] {
        assert!(q(source).prefilter(&refx("a", Harness::Claude, None, None, 3)));
        assert!(!q(source).prefilter(&refx("a", Harness::Claude, None, None, 4)));
    }
    let mut r = refx("a", Harness::Claude, None, None, 1);
    for source in ["updated:2026-01-01..2026-01-02", "updated=2026-01-01..2026-01-02"] {
        r.updated_at = "2026-01-02T23:59:59.999999999Z".parse().ok();
        assert!(q(source).prefilter(&r), "{source}");
        r.updated_at = "2026-01-03T00:00:00Z".parse().ok();
        assert!(!q(source).prefilter(&r), "{source}");
    }
    r.updated_at = "2026-01-02T12:00:00.000000001Z".parse().ok();
    assert!(!q("updated:2026-01-01..2026-01-02T12:00:00Z").prefilter(&r));
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
    assert!(query.needs_counted() && !query.needs_parse());
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
    assert!(query.needs_counted() && !query.needs_parse());
    let mut s = sess(None, &[]);
    s.git = Some(GitInfo {
        branch: Some("lazy-content-ir".into()),
        commit: None,
        remote: None,
    });
    assert!(query.matches_session(&s));
}

#[test]
fn git_equality_matches_one_branch_or_remote() {
    let mut session = sess(None, &[]);
    session.git = Some(GitInfo {
        branch: Some("Feature-Main".into()),
        remote: Some("https://host/Repo".into()),
        commit: None,
    });
    assert!(q("git:main").matches_session(&session));
    assert!(!q("git=main").matches_session(&session));
    assert!(q("git=FEATURE-MAIN").matches_session(&session));
    assert!(q("git=https://host/repo").matches_session(&session));
    assert!(!q("git=\"feature-main https://host/repo\"").matches_session(&session));
}

#[test]
fn text_operators_match_full_text_schema_and_phrase_contract() {
    for source in ["text=word", "content=word", "text>word"] {
        assert!(SessionQuery::parse(source).unwrap_err().contains("only takes `:`"));
    }
    assert!(q("text:\"two words\"").needs_index());
    let schema = schema_json();
    let text = schema["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|field| field["name"] == "text")
        .unwrap();
    assert_eq!(text["ops"], serde_json::json!([":"]));
}

#[test]
fn partially_resolved_models_fall_back_per_needle() {
    struct Fx;
    impl ExtFacts for Fx {
        fn model(&self, needle: &str, exact: bool) -> anyhow::Result<Tri> {
            Ok({
                match needle {
                    "known" => Tri::False,
                    "resolved" => Tri::of(exact),
                    _ => Tri::Unknown,
                }
            })
        }
    }
    let r = refx("a", Harness::Claude, None, None, 1);
    let mut session = Session::new("a", Harness::Claude);
    session.model = Some("known".into());
    let facts = Facts {
        r: &r,
        session: Some(&session),
        ext: &Fx,
    };
    // Falling back for the whole OR-list would incorrectly rediscover the authoritative false
    // needle in Session.model. Unknown is resolved only for that particular needle.
    assert_eq!(q("model:known,missing").eval(&facts).unwrap(), Tri::False);
    assert_eq!(q("model=resolved,missing").eval(&facts).unwrap(), Tri::True);
    assert_eq!(q("model:resolved,missing").eval(&facts).unwrap(), Tri::False);
    session.model = Some("missing".into());
    assert!(q("model:known,missing")
        .matches(&Facts {
            r: &r,
            session: Some(&session),
            ext: &Fx
        })
        .unwrap());
    assert_eq!(
        q("model:known,missing")
            .eval(&Facts {
                r: &r,
                session: None,
                ext: &Fx
            })
            .unwrap(),
        Tri::Unknown
    );
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
        fn tool(&self, n: &str) -> anyhow::Result<Tri> {
            Ok(Tri::of(n == "bash"))
        }
        fn has(&self, f: &str) -> anyhow::Result<Tri> {
            Ok(Tri::of(f == "errors"))
        }
    }
    let query = q("tool:Bash has:errors");
    let r = refx("a", Harness::Claude, None, None, 1);
    let facts = Facts {
        r: &r,
        session: None,
        ext: &Fx,
    };
    assert!(query.matches(&facts).unwrap());
    let query2 = q("tool:Grep");
    assert!(!query2.matches(&facts).unwrap()); // Fx.tool("grep") = False
}

#[test]
fn forest_fields_classify_and_resolve() {
    let query = q("agent:Explore subtool:Bash agents>=3 workflows>=1 compactions>=2 workflow:census has:compacted");
    assert!(query.needs_forest());
    // forest leaves are Unknown in the cheap prefilter → must not exclude
    assert!(query.prefilter(&refx("a", Harness::Claude, None, None, 1)));

    struct Fx;
    impl ExtFacts for Fx {
        fn subtool(&self, n: &str) -> anyhow::Result<Tri> {
            Ok(Tri::of(n == "bash"))
        }
        fn agent_type(&self, n: &str) -> anyhow::Result<Tri> {
            Ok(Tri::of(n == "explore"))
        }
        fn workflow(&self, n: &str) -> anyhow::Result<Tri> {
            Ok(Tri::of(n == "census"))
        }
        fn has(&self, f: &str) -> anyhow::Result<Tri> {
            Ok(Tri::of(f == "compacted"))
        }
        fn count(&self, field: FieldId) -> anyhow::Result<Option<u64>> {
            Ok({
                Some(match field {
                    FieldId::Agents => 4,
                    FieldId::Workflows => 1,
                    FieldId::Compactions => 2,
                    _ => return Ok(None),
                })
            })
        }
    }
    let r = refx("a", Harness::Claude, None, None, 1);
    let facts = Facts {
        r: &r,
        session: None,
        ext: &Fx,
    };
    assert!(query.matches(&facts).unwrap());

    // a count that fails the comparison flips it false
    let q2 = q("agents>=10");
    assert!(!q2.matches(&facts).unwrap()); // Fx says 4
}

#[test]
fn errors_teach() {
    // The retired sub-languages name their replacement instead of silently becoming needles.
    let err = SessionQuery::parse("title~^fix").unwrap_err();
    assert!(
        err.contains("`~`") && err.contains("`:` is substring, `=` exact") && err.contains("see `cv schema`"),
        "{err}"
    );
    let err = SessionQuery::parse(r#"thread:"user > assistant""#).unwrap_err();
    assert!(
        err.contains("thread:") && err.contains("select the session") && err.contains("`cv show`"),
        "{err}"
    );
    assert!(SessionQuery::parse("path:x").unwrap_err().contains("thread:"));
    // A bare `~` word is a needle, not an operator.
    assert!(!q("~/dev/cv").needs_parse());
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
    // An unknown key is a needle too — including a near-miss of a real field (no typo guard).
    let query = q("harnes:claude");
    assert!(query.prefilter(&refx("a", Harness::Claude, None, Some("harnes:claude typo"), 1)));
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
    let extra = |role: Role| bare_message(role, None);
    s.messages.push(extra(Role::User));
    s.messages.push(extra(Role::Tool));
    s.messages.push(extra(Role::System));
    assert!(q("msgs:3").matches_session(&s)); // 2 assistant + 1 user
    assert!(!q("msgs:5").matches_session(&s)); // NOT all five records
}

// --- helpers ---

fn bare_message(role: Role, model: Option<&str>) -> Message {
    Message {
        complete_record: None,
        id: None,
        parent_id: None,
        role,
        timestamp: None,
        model: model.map(str::to_string),
        content: vec![],
        usage: None,
        extra: serde_json::Map::new(),
        kind: crate::ir::MessageKind::for_role(role),
        origin: crate::ir::Origin::for_role(role),
    }
}

fn sess(primary: Option<&str>, turn_models: &[Option<&str>]) -> Session {
    Session {
        model: primary.map(str::to_string),
        messages: turn_models.iter().map(|m| bare_message(Role::Assistant, *m)).collect(),
        ..Session::new("s", Harness::Claude)
    }
}

/// `has:<affordance>` reads the affordance view of the parsed session: a Codex `update_plan` call
/// is a plan whatever the harness calls it, and a session with none is not one.
#[test]
fn has_an_affordance_reads_the_view() {
    use crate::ir::Block;
    let r = refx("a", Harness::Codex, None, None, 1);
    let mut reply = Message::new(Role::Assistant);
    reply.content.push(Block::ToolUse {
        id: "c1".into(),
        name: "update_plan".into(),
        input: serde_json::json!({"plan": [{"step": "x", "status": "pending"}]}).into(),
        namespace: None,
    });
    let s = Session {
        messages: vec![reply],
        ..Session::new("a", Harness::Codex)
    };
    let none = facts::TextSets::empty();
    assert!(facts::matches_full(&q("has:plan"), &r, &s, &none).unwrap());
    assert!(!facts::matches_full(&q("has:schedule"), &r, &s, &none).unwrap());
    assert!(SessionQuery::parse("has:nonsense").is_err());
}

/// `has:` is two tiers: an affordance is counted in the catalog (no parse once counted), a
/// capability flag reads events; a mix needs both.
#[test]
fn has_an_affordance_is_counted_not_events() {
    let plan = q("has:plan,schedule");
    assert!(plan.needs_counted() && !plan.needs_events());
    let errs = q("has:errors");
    assert!(errs.needs_events() && !errs.needs_counted());
    let mixed = q("has:plan has:errors");
    assert!(mixed.needs_events() && mixed.needs_counted());
}

#[test]
fn consulted_fact_errors_propagate_through_negation_and_boolean_terms() {
    struct Broken;
    impl ExtFacts for Broken {
        fn tool(&self, _: &str) -> anyhow::Result<Tri> {
            anyhow::bail!("source changed")
        }
    }
    let r = refx("a", Harness::Claude, None, None, 1);
    let facts = Facts {
        r: &r,
        session: None,
        ext: &Broken,
    };
    for source in ["tool:edit", "NOT tool:edit", "tool:edit OR id:a", "id:a AND tool:edit"] {
        assert_eq!(
            q(source).matches(&facts).unwrap_err().to_string(),
            "source changed",
            "{source}"
        );
    }
    // Keep existing order-sensitive short circuiting: these leaves are never consulted.
    assert!(q("id:a OR tool:edit").matches(&facts).unwrap());
    assert!(!q("id:absent AND tool:edit").matches(&facts).unwrap());
}

#[test]
fn created_is_never_answered_from_the_update_time() {
    // The listing knows only when the session was last written; it was created in January.
    let r = refx("a", Harness::Claude, None, None, 1);
    let mut session = Session::new("a", Harness::Claude);
    session.created_at = "2026-01-01T00:00:00Z".parse().ok();
    let listing = Facts::from_ref(&r);
    let parsed = Facts {
        r: &r,
        session: Some(&session),
        ext: &NoExt,
    };
    assert_eq!(q("created>2026-05-01").eval(&listing).unwrap(), Tri::Unknown);
    assert_eq!(q("created>2026-05-01").eval(&parsed).unwrap(), Tri::False);
    assert_eq!(q("created<2026-02-01").eval(&parsed).unwrap(), Tri::True);
    session.created_at = None;
    let parsed = Facts {
        r: &r,
        session: Some(&session),
        ext: &NoExt,
    };
    assert_eq!(q("created>2026-05-01").eval(&parsed).unwrap(), Tri::False);
}
