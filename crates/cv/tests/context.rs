//! Context projection through both CLI and the shared read API, using isolated harness stores.
use cv_testkit::{claude, World};
use serde_json::{json, Value};

fn usage_line(id: &str, response: &str, usage: Value) -> Value {
    let mut line = claude::assistant(
        id,
        "2026-01-01T00:00:00Z",
        "claude-test",
        json!([{"type":"text","text":"reply"}]),
    );
    line["message"]["id"] = json!(response);
    line["message"]["usage"] = usage;
    line
}

fn timeline(w: &World) {
    let mut estimated = usage_line("estimated", "response-2", json!({"input_tokens":20,"output_tokens":4}));
    estimated["_cv_orig_ctx"] = json!(120);
    w.write_session("context-view", &[
        claude::user("context-view", "question", "2026-01-01T00:00:00Z", "hello"),
        usage_line("one", "response-1", json!({"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":5,"output_tokens":8})),
        usage_line("split", "response-1", json!({"input_tokens":10,"cache_read_input_tokens":100,"cache_creation_input_tokens":5,"output_tokens":8})),
        json!({"type":"system","subtype":"compact_boundary","uuid":"boundary","compactMetadata":{"trigger":"auto","preTokens":120}}),
        estimated,
        usage_line("output-only", "response-3", json!({"output_tokens":2})),
    ]);
}

fn rows(w: &World, extra: &[&str]) -> Value {
    let mut args = vec!["show", "context-view", "--kind", "context", "--json"];
    args.extend(extra);
    serde_json::from_str(&w.cv_ok(&args).0).unwrap()
}

#[test]
fn context_view_counts_calls_once_keeps_unknowns_and_labels_saved_estimates() {
    let w = World::new("context-view");
    timeline(&w);
    let rows = rows(&w, &[]);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 4, "{rows:#?}");
    let (schema, _) = w.cv_ok(&["schema", "--json"]);
    let schema: Value = serde_json::from_str(&schema).unwrap();
    let keys: std::collections::BTreeSet<_> = schema["context_row"]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key.as_str().unwrap())
        .collect();
    for row in rows {
        assert_eq!(
            row.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            keys
        );
        assert!(schema["context_row"]["kind"].as_array().unwrap().contains(&row["kind"]));
        assert!(schema["context_row"]["usage_basis"]
            .as_array()
            .unwrap()
            .contains(&row["usage_basis"]));
    }
    assert_eq!(rows[0]["index"], 1);
    assert_eq!(rows[0]["prompt_tokens"], 115);
    assert_eq!(rows[0]["usage"]["cache_read_tokens"], 100);
    assert_eq!(rows[0]["usage_basis"], "recorded");
    assert_eq!(rows[1]["kind"], "compaction_boundary");
    assert_eq!(rows[1]["pre_compaction_tokens"], 120);
    assert_eq!(rows[2]["usage_basis"], "estimated");
    assert_eq!(rows[2]["prompt_tokens"], 20);
    assert!(rows[3]["prompt_tokens"].is_null());
    assert!(rows.iter().all(|r| r["context_window"].is_null()));
    let (out, _) = w.cv_ok(&["show", "context-view", "--kind", "context"]);
    assert!(out.contains("prompt=115"), "{out}");
    assert!(out.contains("estimated"), "{out}");
}

#[test]
fn context_view_api_matches_projected_cli_windows() {
    let w = World::in_process("context-api", cv_core::env::Env::enter_isolated);
    timeline(&w);
    let path = &["api", "session", "claude", "context-view", "context"];
    for (flags, query) in [
        (vec![], ""),
        (vec!["--last", "2"], "last=2"),
        (vec!["--first", "1"], "first=1"),
        (vec!["--range", "1..3"], "start=1&end=3"),
        (
            vec!["--around", "boundary", "--context", "1"],
            "around=boundary&context=1",
        ),
    ] {
        let (status, body) = cv_core::api::route_json(path, query);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body, rows(&w, &flags), "{query}");
    }
    for query in [
        "last=2&start=1",
        "first=2&around=1",
        "end=1&start=2",
        "context=2",
        "last=oops",
    ] {
        assert_eq!(cv_core::api::route_json(path, query).0, 400, "{query}");
    }
    assert!(rows(&w, &["--last", "0"]).as_array().unwrap().is_empty());
    assert_eq!(rows(&w, &["--pre-compaction"])[0]["index"], 1);
    let limited = rows(&w, &["--max-bytes", "1"]);
    assert_eq!(
        limited.as_array().unwrap().len(),
        1,
        "first projected row remains whole"
    );
}

#[test]
fn context_view_empty_session_has_no_invented_counts() {
    let w = World::new("context-empty");
    w.write_session(
        "context-view",
        &[claude::user("context-view", "u", "2026-01-01T00:00:00Z", "hello")],
    );
    assert_eq!(rows(&w, &[]), json!([]));
    let (out, _) = w.cv_ok(&["show", "context-view", "--kind", "context"]);
    assert!(out.contains("No usage"), "{out}");
}

#[test]
fn context_view_codex_repeated_cumulative_snapshot_is_not_another_call() {
    let w = World::new("context-codex");
    let dir = w.home.join(".codex/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let tc = |total, last| {
        json!({"type":"event_msg","payload":{"type":"token_count","info":{
            "total_token_usage":{"input_tokens":total,"output_tokens":0,"total_tokens":total},
            "last_token_usage":{"input_tokens":last,"cached_input_tokens":10,"output_tokens":0,"total_tokens":last},
            "model_context_window":258400
        }}})
    };
    let reply = |text| json!({"type":"event_msg","payload":{"type":"agent_message","message":text}});
    let lines = [
        json!({"type":"session_meta","payload":{"id":"context-codex","cwd":"/work/test"}}),
        reply("one"),
        tc(100, 100),
        json!({"type":"event_msg","payload":{"type":"user_message","message":"again"}}),
        reply("two"),
        tc(100, 100),
        tc(250, 150),
    ];
    let path = dir.join("rollout-context-codex.jsonl");
    std::fs::write(
        &path,
        lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n",
    )
    .unwrap();
    let (out, _) = w.cv_ok(&[
        "show",
        "context-codex",
        "--harness",
        "codex",
        "--kind",
        "context",
        "--json",
    ]);
    let rows: Vec<Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(rows.iter().filter(|r| r["usage"].is_object()).count(), 2, "{out}");
    assert_eq!(
        rows.iter().filter_map(|r| r["prompt_tokens"].as_u64()).sum::<u64>(),
        250
    );
    assert_eq!(rows[0]["context_window"], 258400);
    assert_eq!(rows[0]["usage"]["input_tokens"], 90);
    assert_eq!(rows[0]["usage"]["cache_read_tokens"], 10);
}
