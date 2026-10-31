//! `--plan`: each action, the refusals, the mirror, attachment drops and revive pricing.
use super::*;
use serde_json::json;

const SID: &str = "44444444-4444-4444-8444-444444444444";

fn record(uuid: &str, parent: Option<&str>, ty: &str, extra: Value) -> Value {
    let mut v = json!({"type": ty, "sessionId": SID, "uuid": uuid, "parentUuid": parent,
        "timestamp": "2026-10-30T00:00:00Z"});
    v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    v
}
fn call(uuid: &str, parent: &str, id: &str) -> Value {
    record(
        uuid,
        Some(parent),
        "assistant",
        json!({"message": {"role": "assistant", "model": "claude-opus-5", "content": [
        {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": format!("run {id}")}}]}}),
    )
}
fn result(uuid: &str, parent: &str, id: &str, content: Value, mirror: Value) -> Value {
    record(
        uuid,
        Some(parent),
        "user",
        json!({"toolUseResult": mirror, "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": id, "content": content}]}}),
    )
}
fn hook(uuid: &str, parent: &str, kind: &str) -> Value {
    record(
        uuid,
        Some(parent),
        "attachment",
        json!({"attachment": {"type": kind, "content": "Session status updated."},
        "rendered": [{"content": "<system-reminder>hook success</system-reminder>"}]}),
    )
}

/// A prompt, then `toolu_a` (multi-line text), `toolu_b` (text) and `toolu_c` (an image), each a
/// call and a result, then a reply. Every result is recent: `keep_last` would spare them all.
fn session(dir: &Path, extra: &[Value]) -> (PathBuf, String) {
    let text_a: String = (0..200).map(|i| format!("line {i}: the build said ok\n")).collect();
    let mut lines = vec![
        record(
            "u0",
            None,
            "user",
            json!({"message": {"role": "user", "content": "go"}}),
        ),
        call("a1", "u0", "toolu_a"),
        result("r1", "a1", "toolu_a", json!(text_a), json!({"stdout": text_a})),
        call("a2", "r1", "toolu_b"),
        result(
            "r2",
            "a2",
            "toolu_b",
            json!("B".repeat(5000)),
            json!({"stdout": "B".repeat(5000)}),
        ),
        call("a3", "r2", "toolu_c"),
        result(
            "r3",
            "a3",
            "toolu_c",
            json!([{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "QUJD"}}]),
            Value::Null,
        ),
    ];
    lines.extend_from_slice(extra);
    let path = dir.join(format!("{SID}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    (path, text_a)
}

fn opts(plan: Value) -> PruneOptions {
    PruneOptions {
        plan: Some(Plan::parse(&plan.to_string()).unwrap()),
        new_id: Some("planned".into()),
        ..Default::default()
    }
}
fn lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
fn by_uuid<'a>(rows: &'a [Value], uuid: &str) -> &'a Value {
    rows.iter().find(|r| r["uuid"] == uuid).unwrap()
}
fn content(row: &Value) -> &str {
    row["message"]["content"][0]["content"].as_str().unwrap()
}

#[test]
fn precis_replaces_with_marker_and_precis_and_leaves_unlisted_results_verbatim() {
    let world = crate::test_world("prune-plan");
    let (src, text_a) = session(&world.proj, &[]);
    let r = prune_session(
        crate::Harness::Claude,
        &src,
        &opts(json!({"results": {"toolu_a": {"action": "precis", "precis": "200 lines of ok"}}})),
    )
    .unwrap();
    let rows = lines(&r.new_path);
    let replaced = content(by_uuid(&rows, "r1"));
    let (marker, precis) = replaced.split_once('\n').unwrap();
    assert!(
        marker.starts_with(MARKER_PREFIX) && marker.contains("cv cat planned toolu_a"),
        "{marker}"
    );
    assert_eq!(precis, "200 lines of ok");
    // Unlisted: verbatim, though toolu_b is over --min-size.
    assert_eq!(content(by_uuid(&rows, "r2")), "B".repeat(5000));
    let sidecar = r.sidecar_path.unwrap();
    assert_eq!(retrieve(&sidecar, "toolu_a").unwrap(), json!(text_a));
    let report = r.plan.unwrap();
    assert_eq!((report.keep, report.precis, report.excerpt), (0, 1, 0));
    assert!(report.bytes_saved > 0);
    assert_eq!(r.pruned_count, 2, "the result and its mirror");
}

#[test]
fn keep_leaves_the_record_untouched() {
    let world = crate::test_world("prune-plan");
    let (src, text_a) = session(&world.proj, &[]);
    let source = lines(&src);
    let r = prune_session(
        crate::Harness::Claude,
        &src,
        &opts(json!({"results": {"toolu_a": {"action": "keep"}}})),
    )
    .unwrap();
    let rows = lines(&r.new_path);
    assert_eq!(content(by_uuid(&rows, "r1")), text_a);
    assert_eq!(
        by_uuid(&rows, "r1")["toolUseResult"],
        by_uuid(&source, "r1")["toolUseResult"]
    );
    assert_eq!(r.pruned_count, 0);
    assert!(r.sidecar_path.is_none());
    assert_eq!(r.plan.unwrap().keep, 1);
}

#[test]
fn excerpt_keeps_passages_verbatim_in_payload_order() {
    let world = crate::test_world("prune-plan");
    let (src, text_a) = session(&world.proj, &[]);
    let plan = json!({"results": {"toolu_a": {"action": "excerpt", "precis": "build log",
        "keep": ["line 150: the build said ok", "line 3: the build said ok\nline 4"]}}});
    let r = prune_session(crate::Harness::Claude, &src, &opts(plan)).unwrap();
    let rows = lines(&r.new_path);
    let replaced = content(by_uuid(&rows, "r1"));
    let marker = replaced.lines().next().unwrap();
    assert!(marker.starts_with(MARKER_PREFIX));
    assert_eq!(
        &replaced[marker.len()..],
        "\nbuild log\n[…]\nline 3: the build said ok\nline 4\n[…]\nline 150: the build said ok"
    );
    assert_eq!(
        retrieve(r.sidecar_path.as_ref().unwrap(), "toolu_a").unwrap(),
        json!(text_a)
    );
    assert_eq!(r.plan.unwrap().excerpt, 1);
}

#[test]
fn a_passage_missing_from_its_payload_refuses_before_writing() {
    let world = crate::test_world("prune-plan");
    let (src, _) = session(&world.proj, &[]);
    let long = format!("{} not in the log", "x".repeat(100));
    let plan = json!({"results": {
        "toolu_a": {"action": "excerpt", "keep": ["line 1: the build said ok", "line 9999", long]},
        "toolu_c": {"action": "excerpt", "keep": []}}});
    let err = prune_session(crate::Harness::Claude, &src, &opts(plan)).unwrap_err();
    let refused = err.downcast_ref::<PlanRefused>().expect("a plan refusal");
    let text = refused.to_string();
    assert_eq!(refused.0.len(), 3, "{text}");
    assert!(text.contains("toolu_a") && text.contains("\"line 9999\""), "{text}");
    assert!(
        text.contains(&format!("{:?}…", "x".repeat(80))),
        "the first 80 chars: {text}"
    );
    assert!(
        text.contains("toolu_c: an excerpt needs a text payload; this one is image"),
        "{text}"
    );
    assert!(!world.proj.join("planned.jsonl").exists());
    assert!(!world.proj.join("planned.flat.jsonl").exists());
}

#[test]
fn unknown_ids_are_refused_all_at_once() {
    let world = crate::test_world("prune-plan");
    let (src, _) = session(&world.proj, &[]);
    let plan = json!({"results": {
        "toolu_zz": {"action": "keep"},
        "toolu_a": {"action": "precis", "precis": "ok"},
        "toolu_yy": {"action": "precis", "precis": "?"}}});
    let mut options = opts(plan);
    options.dry_run = true;
    let err = prune_session(crate::Harness::Claude, &src, &options).unwrap_err();
    let refused = err.downcast_ref::<PlanRefused>().expect("a plan refusal");
    assert_eq!(
        refused.0,
        [
            "toolu_yy: no tool_result with this id in the session",
            "toolu_zz: no tool_result with this id in the session",
        ]
    );
}

#[test]
fn a_planned_results_mirror_is_stashed_whatever_its_size_and_unplanned_mirrors_stay() {
    let world = crate::test_world("prune-plan");
    let (src, _) = session(&world.proj, &[]);
    let source = lines(&src);
    let plan = json!({"results": {"toolu_a": {"action": "precis", "precis": "ok"},
        "toolu_c": {"action": "precis", "precis": "a screenshot of the login page"}}});
    let r = prune_session(crate::Harness::Claude, &src, &opts(plan)).unwrap();
    let rows = lines(&r.new_path);
    let mirror = by_uuid(&rows, "r1")["toolUseResult"].as_str().unwrap();
    assert!(mirror.starts_with("[PRUNED id=toolu_a#tur"), "{mirror}");
    let sidecar = r.sidecar_path.unwrap();
    assert_eq!(
        retrieve(&sidecar, "toolu_a#tur").unwrap(),
        by_uuid(&source, "r1")["toolUseResult"]
    );
    // An unplanned result keeps its mirror, though it is over --min-size.
    assert_eq!(
        by_uuid(&rows, "r2")["toolUseResult"],
        by_uuid(&source, "r2")["toolUseResult"]
    );
    // A précis may stand in for an image; the marker says what it was.
    let image = content(by_uuid(&rows, "r3"));
    assert!(
        image.contains("| image ") && image.ends_with("\na screenshot of the login page"),
        "{image}"
    );
    assert_eq!(r.image_blocks, 1);
    assert_eq!(
        by_uuid(&rows, "r3")["toolUseResult"],
        Value::Null,
        "a null mirror stays null"
    );
}

#[test]
fn dropped_attachments_are_stashed_and_their_children_relinked() {
    let world = crate::test_world("prune-plan");
    let tail = [
        hook("h1", "r3", "hook_success"),
        hook("h2", "h1", "hook_success"),
        hook("h3", "h2", "edited_text_file"),
        record(
            "a4",
            Some("h3"),
            "assistant",
            json!({"message": {"role": "assistant", "model": "claude-opus-5",
            "content": [{"type": "text", "text": "done"}]}}),
        ),
        hook("h4", "a4", "hook_success"),
        record(
            "u5",
            Some("h4"),
            "user",
            json!({"message": {"role": "user", "content": "thanks"}}),
        ),
    ];
    let (src, _) = session(&world.proj, &tail);
    let source = lines(&src);
    let plan = json!({"attachments": {"drop_kinds": ["hook_success"]}});
    let r = prune_session(crate::Harness::Claude, &src, &opts(plan)).unwrap();
    let rows = lines(&r.new_path);
    let uuids: Vec<&str> = rows.iter().map(|r| r["uuid"].as_str().unwrap()).collect();
    assert_eq!(uuids, ["u0", "a1", "r1", "a2", "r2", "a3", "r3", "h3", "a4", "u5"]);
    assert_eq!(by_uuid(&rows, "h3")["parentUuid"], "r3");
    assert_eq!(by_uuid(&rows, "u5")["parentUuid"], "a4");
    // The chain from the newest record reaches the root through records that exist.
    let parents: HashMap<&str, Option<&str>> = rows
        .iter()
        .map(|r| (r["uuid"].as_str().unwrap(), r["parentUuid"].as_str()))
        .collect();
    let (mut at, mut walked) = (Some("u5"), 0);
    while let Some(uuid) = at {
        at = *parents.get(uuid).expect("every parent is a record of the session");
        walked += 1;
    }
    assert_eq!(walked, rows.len());
    crate::harness::claude::emit::verify_claude_resumable(&r.new_path, None).unwrap();
    assert_eq!(
        retrieve(r.sidecar_path.as_ref().unwrap(), "h2").unwrap(),
        by_uuid(&source, "h2").clone()
    );
    let report = r.plan.unwrap();
    assert_eq!(report.attachments_dropped, 3);
    assert!(report.bytes_saved > 0);
    assert_eq!(r.pruned_count, 0, "attachments are not tool payloads");
}

#[test]
fn revive_prices_the_replacement_not_the_old_payload() {
    let world = crate::test_world("prune-plan");
    let reply = record(
        "a4",
        Some("r3"),
        "assistant",
        json!({"message": {"role": "assistant", "model": "claude-opus-5",
        "content": [{"type": "text", "text": "done"}],
        "usage": {"input_tokens": 2, "cache_read_input_tokens": 200_000, "cache_creation_input_tokens": 0}}}),
    );
    let (src, text_a) = session(&world.proj, &[reply]);
    // After a compaction the stale figure is no evidence: revive estimates the branch outright.
    let boundary = json!({"type": "system", "subtype": "compact_boundary", "sessionId": SID, "uuid": "b0",
        "parentUuid": null, "timestamp": "2026-10-30T00:00:00Z"});
    let body = std::fs::read_to_string(&src).unwrap();
    std::fs::write(&src, format!("{boundary}\n{body}")).unwrap();
    let revived = |plan: Option<Value>| {
        let mut options = PruneOptions {
            revive: true,
            dry_run: true,
            ..Default::default()
        };
        options.plan = plan.map(|p| Plan::parse(&p.to_string()).unwrap());
        prune_session(crate::Harness::Claude, &src, &options)
            .unwrap()
            .revive_tokens
            .unwrap()
    };
    let precis = |text: &str| Some(json!({"results": {"toolu_a": {"action": "precis", "precis": text}}}));
    let unpruned = revived(None);
    let short = revived(precis("ok"));
    let long = revived(precis(&text_a[..text_a.len() / 2]));
    assert!(short < long && long < unpruned, "{short} < {long} < {unpruned}");
}
