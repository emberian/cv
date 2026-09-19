use super::*;

/// A tiny but realistic Claude JSONL: an assistant tool_use, a user tool_result with a big payload
/// (both the content block and the toolUseResult mirror), then a recent turn that must be spared.
fn fixture(dir: &Path, big: &str) -> PathBuf {
    let sid = "11111111-1111-4111-8111-111111111111";
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","timestamp":"2026-06-16T00:00:00Z",
            "message":{"role":"user","content":"do the thing"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","parentUuid":"u0","timestamp":"2026-06-16T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/big.log"}}]}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","parentUuid":"a1","timestamp":"2026-06-16T00:00:02Z",
            "toolUseResult":{"stdout":big},
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":big}]}}),
        // a recent assistant turn (within keep-last) — must be left verbatim
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a2","parentUuid":"u1","timestamp":"2026-06-16T00:00:03Z",
            "message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_2","name":"Read","input":{"file_path":"/recent.log"}}]}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u2","parentUuid":"a2","timestamp":"2026-06-16T00:00:04Z",
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_2","content":big}]}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    let body: String = lines.iter().map(|l| l.to_string() + "\n").collect();
    std::fs::write(&path, body).unwrap();
    path
}

fn tmpdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("cv-prune-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn assistant_usage(total: u64) -> Value {
    serde_json::json!({"type":"assistant",
        "message":{"usage":{"input_tokens":total,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}})
}

/// Parse fixture values into the shared one-pass representation the sizing helpers read.
fn pv(lines: &[Value]) -> Vec<Option<Value>> {
    lines.iter().cloned().map(Some).collect()
}

#[test]
fn window_cutoff_sizes_by_real_recorded_usage() {
    // 5 assistant turns, cumulative usage 100k..500k (one compaction segment) — 100k per turn.
    let lines: Vec<Value> = (1..=5).map(|k| assistant_usage(k * 100_000)).collect();
    let parsed = pv(&lines);
    // budget 250k → the LARGEST tail whose real size stays ≤ 250k is turns 3,4 (200k real).
    // (Keeping turn 2 as well would be 300k — over budget; the contract is ≤, never ≥.)
    assert_eq!(usage_window_cutoff(&parsed, 250_000), Some((3, 200_000, false)));
    // budget beyond the whole session → keep everything.
    assert_eq!(usage_window_cutoff(&parsed, 9_000_000), Some((0, 500_000, false)));
    // no usage records → None (caller falls back to a byte estimate).
    assert_eq!(
        usage_window_cutoff(&pv(&[serde_json::json!({"type":"user"})]), 100),
        None
    );
}

#[test]
fn window_cutoff_keeps_single_huge_turn_but_flags_overshoot() {
    let lines: Vec<Value> = (1..=5).map(|k| assistant_usage(k * 100_000)).collect();
    // Even the newest turn alone (100k) busts a 50k budget: keep that one turn, flag it — the
    // caller must warn that the ≤-budget contract could not be honored.
    assert_eq!(usage_window_cutoff(&pv(&lines), 50_000), Some((4, 100_000, true)));
}

#[test]
fn window_cutoff_skips_sidechain_usage() {
    // A sub-agent (isSidechain) turn carries a HUGE usage from its own context window. It must not
    // poison the main thread's sizing: with it ignored, the tail is sized purely by the main turns.
    let mut side = assistant_usage(950_000);
    side["isSidechain"] = Value::Bool(true);
    let lines = [assistant_usage(100_000), side, assistant_usage(200_000)];
    // Main turns: 0 (100k) and 1 (200k → delta 100k). Budget 150k → keep from main turn 1.
    assert_eq!(usage_window_cutoff(&pv(&lines), 150_000), Some((1, 100_000, false)));
}

#[test]
fn window_cutoff_reads_stashed_usage_after_revive() {
    // A revived session: usage was pinned to 50k, but the real count is in `_cv_orig_ctx`.
    let mk = |orig: u64| {
        serde_json::json!({"type":"assistant","_cv_orig_ctx":orig,
            "message":{"usage":{"input_tokens":50_000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}})
    };
    let lines = [mk(100_000), mk(200_000), mk(300_000)];
    // Sizing must use the stashed 100/200/300k (100k per turn), NOT the pinned 50k — else windowing
    // a chained (already-revived) session would size by garbage. Budget 150k fits one turn (100k).
    assert_eq!(usage_window_cutoff(&pv(&lines), 150_000), Some((2, 100_000, false)));
}

#[test]
fn prunes_old_payloads_into_new_session_keeping_recent() {
    let dir = tmpdir();
    let big = "X".repeat(5000); // > default min_size 2048
    let src = fixture(&dir, &big);

    let opts = PruneOptions {
        min_size: 2048,
        keep_last: 2,
        drop: false,
        thinking: false,
        new_id: None,
        copy_resources: false,
        revive: false,
        dry_run: false,
        ..Default::default()
    };
    let r = prune_session(&src, &opts).unwrap();

    // new session, distinct id + file
    assert_ne!(r.new_id, r.source_id);
    assert!(r.new_path.exists());
    assert!(r.new_size < r.original_size);
    assert!(r.pruned_count >= 1);
    assert!(r.est_context_tokens_saved > 0);

    let out = std::fs::read_to_string(&r.new_path).unwrap();
    // every line stamped with the NEW id, none with the old
    assert!(out.contains(&r.new_id));
    assert!(!out.contains(&r.source_id));
    // the OLD tool_result (toolu_1) is a marker; the big payload is gone from the line
    assert!(out.contains("[PRUNED id=toolu_1"));
    // the RECENT tool_result (toolu_2, within keep_last=2) is spared — its big payload survives
    // somewhere in the output (its tool_result line is left verbatim).
    assert!(out.contains(&big), "recent payload must be kept verbatim");
    let recent_result = out
        .lines()
        .find(|l| l.contains("tool_result") && l.contains("toolu_2"))
        .unwrap();
    assert!(recent_result.contains(&big), "recent tool_result kept");
    // old tool_result line no longer carries the raw payload
    let old_result = out
        .lines()
        .find(|l| l.contains("tool_result") && l.contains("toolu_1"))
        .unwrap();
    assert!(!old_result.contains(&big));

    // sidecar holds the original, retrievable byte-faithfully
    let sc = r.sidecar_path.expect("sidecar written");
    assert!(sc.exists());
    let restored = retrieve(&sc, "toolu_1").unwrap();
    assert_eq!(restored.as_str().unwrap(), big);
    // the toolUseResult mirror was also stashed
    assert!(retrieve(&sc, "toolu_1#tur").is_ok());

    // resulting session still parses as valid JSONL
    for line in out.lines() {
        serde_json::from_str::<serde_json::Value>(line).expect("valid json line");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn drop_mode_writes_no_sidecar_and_never_advertises_retrieve() {
    let dir = tmpdir();
    let big = "Y".repeat(5000);
    let src = fixture(&dir, &big);
    // keep_last 0 → every turn is "old", so the drop removes ALL big payloads.
    let opts = PruneOptions {
        min_size: 2048,
        keep_last: 0,
        drop: true,
        thinking: false,
        new_id: Some("aaaa".into()),
        copy_resources: false,
        revive: false,
        dry_run: false,
        ..Default::default()
    };
    let r = prune_session(&src, &opts).unwrap();
    assert_eq!(r.new_id, "aaaa");
    assert!(r.sidecar_path.is_none());
    assert!(!dir.join("aaaa.flat.jsonl").exists());
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert!(out.contains("[PRUNED id=toolu_1"));
    assert!(!out.contains(&big)); // dropped entirely (nothing retained, no sidecar)
                                  // the payload is destroyed — the marker must say so, not point at a sidecar that doesn't exist
    assert!(
        out.contains("dropped (no sidecar)"),
        "drop markers say the payload is gone"
    );
    assert!(!out.contains("--retrieve"), "drop markers must not advertise retrieval");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn thinking_flatten_targets_old_reasoning_only() {
    let dir = tmpdir();
    let big = "R".repeat(5000);
    let sid = "22222222-2222-4222-8222-222222222222";
    // old assistant turn with big thinking; then enough turns that it's outside keep_last; a recent
    // assistant turn with big thinking that must be preserved.
    let line = |uuid: &str, parent: &str, role: &str, content: serde_json::Value| {
        serde_json::json!({"type":role,"sessionId":sid,"uuid":uuid,"parentUuid":parent,
            "timestamp":"2026-06-18T00:00:00Z","message":{"role":role,"content":content}})
    };
    let think = |t: &str| serde_json::json!([{"type":"thinking","thinking":t,"signature":"sig"}]);
    let lines = [
        line("u0", "", "user", serde_json::json!("start")),
        line("a0", "u0", "assistant", think(&big)), // OLD thinking → flatten
        line("u1", "a0", "user", serde_json::json!("more")),
        line("a1", "u1", "assistant", serde_json::json!("ok")),
        line("u2", "a1", "user", serde_json::json!("again")),
        line("a2", "u2", "assistant", think(&big)), // RECENT thinking → keep (within keep_last)
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    let opts = PruneOptions {
        min_size: 1024,
        keep_last: 2,
        thinking: true,
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    let out = std::fs::read_to_string(&r.new_path).unwrap();

    // old thinking (a0) flattened to a text marker; its big payload gone, original in sidecar
    let a0 = out.lines().find(|l| l.contains("\"uuid\":\"a0\"")).unwrap();
    assert!(
        a0.contains("[PRUNED id=a0#think0"),
        "old thinking flattened (id = message uuid)"
    );
    assert!(!a0.contains(&big));
    assert!(
        !a0.contains("\"thinking\""),
        "thinking block became a text marker (no signature mismatch)"
    );
    // recent thinking (a2, within keep_last=2) preserved verbatim
    let a2 = out.lines().find(|l| l.contains("\"uuid\":\"a2\"")).unwrap();
    assert!(
        a2.contains(&big) && a2.contains("\"thinking\""),
        "recent thinking kept verbatim"
    );
    // retrievable
    let restored = retrieve(r.sidecar_path.as_ref().unwrap(), "a0#think0").unwrap();
    assert_eq!(restored.get("thinking").and_then(|v| v.as_str()), Some(big.as_str()));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn revive_rewrites_stale_usage_below_loaded_content() {
    let dir = tmpdir();
    let sid = "33333333-3333-4333-8333-333333333333";
    // A compaction boundary, then a small post-boundary window whose last assistant turn still
    // carries a giant *recorded* usage (the stale wall) even though the content is tiny.
    let line = |uuid: &str, role: &str, content: serde_json::Value, usage: Option<serde_json::Value>| {
        let mut m = serde_json::json!({"role":role,"content":content});
        if let Some(u) = usage {
            m["usage"] = u;
        }
        serde_json::json!({"type":role,"sessionId":sid,"uuid":uuid,"timestamp":"2026-06-19T00:00:00Z","message":m})
    };
    let stale = serde_json::json!({"input_tokens":2,"output_tokens":10,
        "cache_read_input_tokens":975_000,"cache_creation_input_tokens":296,
        "cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":296}});
    let lines = [
        line("u0", "user", serde_json::json!("old turn"), None),
        serde_json::json!({"type":"system","subtype":"compact_boundary","sessionId":sid,"uuid":"b0",
            "timestamp":"2026-06-19T00:00:01Z"}),
        line("u1", "user", serde_json::json!("hi again"), None),
        line(
            "a1",
            "assistant",
            serde_json::json!([{"type":"text","text":"small reply"}]),
            Some(stale),
        ),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    let opts = PruneOptions {
        revive: true,
        keep_last: 100, // don't snip anything — isolate the revive behavior
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();

    assert_eq!(
        r.usage_rewritten, 1,
        "the one inflated post-boundary usage record corrected"
    );
    assert_eq!(
        r.revive_old_tokens,
        Some(975_298),
        "reports the stale total it replaced"
    );
    let honest = r.revive_tokens.expect("honest figure written");
    assert!(
        honest < 1000,
        "tiny post-boundary content → tiny honest figure (got {honest})"
    );

    // the written file carries the corrected usage, caches zeroed
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    let a1 = out.lines().find(|l| l.contains("\"uuid\":\"a1\"")).unwrap();
    let v: serde_json::Value = serde_json::from_str(a1).unwrap();
    let u = v.pointer("/message/usage").unwrap();
    assert_eq!(u["cache_read_input_tokens"], 0);
    assert_eq!(u["cache_creation_input_tokens"], 0);
    assert_eq!(u["input_tokens"].as_u64().unwrap(), honest);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn usage_total_reads_the_last_request_iteration_like_claude_code() {
    // Claude Code ≥ 2.1.277 sizes context from `usage.iterations` (the last non-advisor/compaction
    // request) — the top-level counters are only a fallback. cv must see the same number.
    let u = |v: Value| usage_total(v.as_object().unwrap());
    let top =
        |t: u64| serde_json::json!({"input_tokens":t,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    // iterations present: the last request iteration wins over a top-level that says otherwise,
    // and a trailing advisor iteration is skipped
    let mut v = top(500_866);
    v["iterations"] = serde_json::json!([
        {"type":"message","input_tokens":2,"output_tokens":78,
         "cache_read_input_tokens":975_473,"cache_creation_input_tokens":1_031},
        {"type":"advisor_message","input_tokens":1,"output_tokens":1,
         "cache_read_input_tokens":9_999_999,"cache_creation_input_tokens":0},
    ]);
    assert_eq!(u(v), 976_506);
    // no iterations → top-level
    assert_eq!(u(top(123)), 123);
    // top-level zero → zero, whatever iterations say (Claude Code short-circuits there)
    let mut z = top(0);
    z["iterations"] = serde_json::json!([{"type":"message","input_tokens":5,"output_tokens":1,
        "cache_read_input_tokens":0,"cache_creation_input_tokens":0}]);
    assert_eq!(u(z), 0);
    // a zero-context or malformed last iteration → top-level fallback
    let mut m = top(321);
    m["iterations"] = serde_json::json!([{"type":"message","input_tokens":0,"output_tokens":1,
        "cache_read_input_tokens":0,"cache_creation_input_tokens":0}]);
    assert_eq!(u(m), 321);
    let mut m2 = top(321);
    m2["iterations"] = serde_json::json!([{"type":"message","input_tokens":7}]);
    assert_eq!(u(m2), 321);
}

#[test]
fn revive_pins_usage_iterations_too() {
    // The regression behind "a pruned session never resumes" on Claude Code 2.1.277+: revive pinned
    // the top-level usage but left `iterations[*]` at the stale wall figure — and that is what the
    // gate reads now, so it refused the (small) session client-side with a synthesized "Prompt is
    // too long" before sending anything.
    let dir = tmpdir();
    let sid = "44444444-4444-4444-8444-444444444444";
    let line = |uuid: &str, role: &str, content: serde_json::Value, usage: Option<serde_json::Value>| {
        let mut m = serde_json::json!({"role":role,"content":content});
        if let Some(u) = usage {
            m["usage"] = u;
        }
        serde_json::json!({"type":role,"sessionId":sid,"uuid":uuid,"timestamp":"2026-09-19T00:00:00Z","message":m})
    };
    // Top level already small (as a pre-fix revive would have left it); the stale wall lives in the
    // request iteration; an advisor iteration rides along and must be left alone.
    let stale = serde_json::json!({"input_tokens":50_000,"output_tokens":10,
    "cache_read_input_tokens":0,"cache_creation_input_tokens":0,
    "cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},
    "iterations":[
        {"type":"message","input_tokens":2,"output_tokens":10,
         "cache_read_input_tokens":975_000,"cache_creation_input_tokens":296,
         "cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":296}},
        {"type":"advisor_message","input_tokens":1,"output_tokens":1,
         "cache_read_input_tokens":777,"cache_creation_input_tokens":0}
    ]});
    let lines = [
        line("u0", "user", serde_json::json!("old turn"), None),
        serde_json::json!({"type":"system","subtype":"compact_boundary","sessionId":sid,"uuid":"b0",
            "timestamp":"2026-09-19T00:00:01Z"}),
        line("u1", "user", serde_json::json!("hi again"), None),
        line(
            "a1",
            "assistant",
            serde_json::json!([{"type":"text","text":"small reply"}]),
            Some(stale),
        ),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    let opts = PruneOptions {
        revive: true,
        keep_last: 100,
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    assert_eq!(r.usage_rewritten, 1);
    assert_eq!(
        r.revive_old_tokens,
        Some(975_298),
        "the stale figure is read from the request iteration, not the (already small) top level"
    );
    let honest = r.revive_tokens.expect("honest figure written");
    assert!(
        honest < 1000,
        "tiny post-boundary content → tiny honest figure (got {honest})"
    );

    let out = std::fs::read_to_string(&r.new_path).unwrap();
    let a1 = out.lines().find(|l| l.contains("\"uuid\":\"a1\"")).unwrap();
    let v: serde_json::Value = serde_json::from_str(a1).unwrap();
    assert_eq!(
        v["_cv_orig_ctx"], 975_298,
        "the real (iteration) figure is stashed for a later --window"
    );
    let u = v.pointer("/message/usage").unwrap();
    assert_eq!(u["input_tokens"].as_u64().unwrap(), honest);
    assert_eq!(u["cache_read_input_tokens"], 0);
    let req = &u["iterations"][0];
    assert_eq!(
        req["input_tokens"].as_u64().unwrap(),
        honest,
        "request iteration pinned"
    );
    assert_eq!(req["cache_read_input_tokens"], 0);
    assert_eq!(req["cache_creation_input_tokens"], 0);
    assert_eq!(req["cache_creation"]["ephemeral_5m_input_tokens"], 0);
    assert_eq!(req["output_tokens"], 10, "output counts are real and untouched");
    let adv = &u["iterations"][1];
    assert_eq!(adv["cache_read_input_tokens"], 777, "advisor iteration left alone");
    // and what Claude Code will read back is the pinned figure
    assert_eq!(usage_total(u.as_object().unwrap()), honest);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn revive_honors_recorded_delta_evidence_over_low_byte_estimate() {
    let dir = tmpdir();
    let sid = "44444444-4444-4444-8444-444444444444";
    // No boundary, nothing snipped in the second span: the recorded usage DELTA (30k between a0 and
    // a1) is hard evidence that span really costs 30k, even though its byte estimate is tiny (e.g.
    // dense code that tokenizes far above 3.5 B/tok). Revive must never write a figure below that
    // evidence — the old byte-only estimator did, and let resumes through that blew the real limit.
    let usage =
        |n: u64| serde_json::json!({"input_tokens":n,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let big = "B".repeat(5000);
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","timestamp":"2026-06-20T00:00:00Z",
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tx","content":big}]}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a0","timestamp":"2026-06-20T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"ok"}],"usage":usage(50_000)}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","timestamp":"2026-06-20T00:00:02Z",
            "message":{"role":"user","content":"tiny"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","timestamp":"2026-06-20T00:00:03Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"tiny reply"}],"usage":usage(80_000)}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    // keep_last 0 → the u0 tool_result is snipped, so its span falls back to a byte estimate; the
    // untouched u1..a1 span keeps its recorded 30k delta as a floor.
    let opts = PruneOptions {
        revive: true,
        keep_last: 0,
        min_size: 2048,
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    assert!(r.pruned_count >= 1, "the big tool_result was snipped");
    let honest = r.revive_tokens.expect("stale records rewritten");
    assert!(
        honest >= 30_000,
        "honest figure must respect the recorded 30k delta evidence, got {honest}"
    );
    assert!(
        honest < 80_000,
        "…but still shed the snipped payload's stale total, got {honest}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// Fixture for the full `--window` behavior: a leading summary (title) record, two prompt/reply
/// pairs with recorded usage, and a trailing non-turn record.
fn window_fixture(dir: &Path) -> PathBuf {
    let sid = "55555555-5555-4555-8555-555555555555";
    let usage =
        |n: u64| serde_json::json!({"input_tokens":n,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let lines = [
        serde_json::json!({"type":"summary","summary":"my session title","leafUuid":"u0"}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","parentUuid":null,"timestamp":"2026-06-21T00:00:00Z",
            "message":{"role":"user","content":"old prompt"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a0","parentUuid":"u0","timestamp":"2026-06-21T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"old reply"}],"usage":usage(100_000)}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","parentUuid":"a0","timestamp":"2026-06-21T00:00:02Z",
            "message":{"role":"user","content":"new prompt"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","parentUuid":"u1","timestamp":"2026-06-21T00:00:03Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"new reply"}],"usage":usage(200_000)}}),
        serde_json::json!({"type":"file-history-snapshot","messageId":"m1","snapshot":{"files":{}}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    path
}

#[test]
fn window_prune_reroots_within_budget_and_keeps_bookend_records() {
    let dir = tmpdir();
    let src = window_fixture(&dir);
    // 120k budget: each pair costs 100k real, so only the newest pair fits (≤ contract).
    let opts = PruneOptions {
        window: Some(120_000),
        ..Default::default()
    };
    let r = prune_session(&src, &opts).unwrap();

    assert_eq!(r.dropped_turns, 2, "old prompt+reply dropped");
    let real = r.window_real_tokens.expect("sized by recorded usage");
    assert!(real <= 120_000, "≤-budget contract: kept tail is {real}, budget 120000");
    assert_eq!(real, 100_000);
    assert!(r.warnings.is_empty(), "no overshoot → no warning");

    let out = std::fs::read_to_string(&r.new_path).unwrap();
    // dropped turns are gone
    assert!(!out.contains("old prompt") && !out.contains("old reply"));
    // the kept tail opens on the user prompt, re-rooted (parentUuid nulled)
    let u1 = out.lines().find(|l| l.contains("\"uuid\":\"u1\"")).unwrap();
    let v: Value = serde_json::from_str(u1).unwrap();
    assert!(v["parentUuid"].is_null(), "first kept turn re-rooted");
    // the summary (title) record survives the head drop; the trailing snapshot survives the tail
    assert!(out.contains("my session title"), "leading summary record kept");
    assert!(out.contains("file-history-snapshot"), "trailing non-turn record kept");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn window_overshoot_keeps_the_huge_turn_and_warns() {
    let dir = tmpdir();
    let sid = "66666666-6666-4666-8666-666666666666";
    // No usage records → byte-estimate path. The NEWEST turn alone dwarfs the budget.
    let huge = "H".repeat(70_000); // ≈ 20k estimated tokens
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","parentUuid":null,"timestamp":"2026-06-22T00:00:00Z",
            "message":{"role":"user","content":"small"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a0","parentUuid":"u0","timestamp":"2026-06-22T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"ok"}]}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","parentUuid":"a0","timestamp":"2026-06-22T00:00:02Z",
            "message":{"role":"user","content":huge}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    let opts = PruneOptions {
        window: Some(10),
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    assert_eq!(r.dropped_turns, 2, "everything but the huge newest turn dropped");
    assert!(!r.warnings.is_empty(), "overshoot must be reported loudly");
    assert!(
        r.warnings[0].contains("EXCEEDS"),
        "warning names the busted budget: {:?}",
        r.warnings
    );
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert!(
        out.contains(&huge),
        "the one oversized turn is still kept — never an empty session"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn window_and_revive_ignore_sidechain_turns() {
    let dir = tmpdir();
    let sid = "77777777-7777-4777-8777-777777777777";
    let usage =
        |n: u64| serde_json::json!({"input_tokens":n,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","parentUuid":null,"timestamp":"2026-06-23T00:00:00Z",
            "message":{"role":"user","content":"old prompt"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a0","parentUuid":"u0","timestamp":"2026-06-23T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"old reply"}],"usage":usage(100_000)}}),
        // a sub-agent turn: its 950k usage measures the SUB-AGENT's context, not ours
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"sc0","parentUuid":null,"isSidechain":true,
            "timestamp":"2026-06-23T00:00:02Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"subagent says hi"}],"usage":usage(950_000)}}),
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","parentUuid":"a0","timestamp":"2026-06-23T00:00:03Z",
            "message":{"role":"user","content":"new prompt"}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","parentUuid":"u1","timestamp":"2026-06-23T00:00:04Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"new reply"}],"usage":usage(200_000)}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    let opts = PruneOptions {
        window: Some(120_000),
        revive: true,
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();

    // Sizing saw main-thread costs of 100k per pair — the sidechain's 950k didn't poison it.
    assert_eq!(r.window_real_tokens, Some(100_000));
    assert_eq!(r.dropped_turns, 2, "only MAIN turns counted as dropped");
    assert!(r.warnings.is_empty());

    let out = std::fs::read_to_string(&r.new_path).unwrap();
    // The sidechain line rides along in the kept region as content…
    let sc = out
        .lines()
        .find(|l| l.contains("\"uuid\":\"sc0\""))
        .expect("sidechain line kept");
    // …but revive never rewrites ITS usage (a different context's number) …
    let scv: Value = serde_json::from_str(sc).unwrap();
    assert_eq!(
        scv.pointer("/message/usage/input_tokens").and_then(Value::as_u64),
        Some(950_000)
    );
    // …and the main-thread stale record was pinned to the honest main figure.
    assert_eq!(r.revive_tokens, Some(100_000));
    let a1: Value = serde_json::from_str(out.lines().find(|l| l.contains("\"uuid\":\"a1\"")).unwrap()).unwrap();
    assert_eq!(
        a1.pointer("/message/usage/input_tokens").and_then(Value::as_u64),
        Some(100_000)
    );
    // The re-root must be the first MAIN turn (u1), never the sidechain line.
    let u1: Value = serde_json::from_str(out.lines().find(|l| l.contains("\"uuid\":\"u1\"")).unwrap()).unwrap();
    assert!(u1["parentUuid"].is_null());
    assert!(
        scv["parentUuid"].is_null(),
        "sidechain parent untouched (was already null)"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sidecar_ids_are_unique_and_round_trip_arrays_and_objects() {
    let dir = tmpdir();
    let sid = "88888888-8888-4888-8888-888888888888";
    let big_a = "A".repeat(4000);
    let big_b = "B".repeat(4000);
    // Two lines whose tool_result blocks carry NO tool_use_id (the collision-prone shape), each
    // with an array content payload and an object toolUseResult mirror.
    let arr = |t: &str| {
        serde_json::json!([
            {"type":"text","text":t},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}
        ])
    };
    let mk = |uuid: &str, t: &str| {
        serde_json::json!({"type":"user","sessionId":sid,"uuid":uuid,"timestamp":"2026-06-24T00:00:00Z",
            "toolUseResult":{"stdout":t,"exit":0},
            "message":{"role":"user","content":[{"type":"tool_result","content":arr(t)}]}})
    };
    let lines = [mk("uA", &big_a), mk("uB", &big_b)];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    let opts = PruneOptions {
        keep_last: 0,
        min_size: 1024,
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    assert_eq!(r.pruned_count, 4, "two content payloads + two mirrors");
    let sc = r.sidecar_path.expect("sidecar written");

    // Every sidecar id is unique — no line ever shadows another's payload.
    let raw = std::fs::read_to_string(&sc).unwrap();
    let ids: Vec<String> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let mut deduped = ids.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), ids.len(), "sidecar ids must be unique, got {ids:?}");

    // Array payloads (text + image blocks) round-trip verbatim, per line.
    assert_eq!(retrieve(&sc, "uA#tr0").unwrap(), arr(&big_a));
    assert_eq!(retrieve(&sc, "uB#tr0").unwrap(), arr(&big_b));
    // Object toolUseResult mirrors round-trip verbatim too.
    assert_eq!(
        retrieve(&sc, "uA#tr0#tur").unwrap(),
        serde_json::json!({"stdout":big_a,"exit":0})
    );
    assert_eq!(
        retrieve(&sc, "uB#tr0#tur").unwrap(),
        serde_json::json!({"stdout":big_b,"exit":0})
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn retrieve_errors_on_duplicate_ids_instead_of_guessing() {
    let dir = tmpdir();
    let sc = dir.join("dup.flat.jsonl");
    let entry = |content: &str| {
        serde_json::json!({"id":"dup","slot":"content","name":"Read","input":null,
            "content":content,"size":content.len(),"line_count":1,"kind":"text"})
        .to_string()
    };
    std::fs::write(
        &sc,
        format!("{}\n{}\n", entry("first payload"), entry("second payload")),
    )
    .unwrap();
    let err = retrieve(&sc, "dup").unwrap_err().to_string();
    assert!(err.contains("2 times"), "duplicate id must be an error, got: {err}");
    // a non-duplicated id in the same file still errors as not-found (with the listing)
    assert!(retrieve(&sc, "absent").is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn double_prune_is_idempotent() {
    let dir = tmpdir();
    let big = "D".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions {
        keep_last: 0,
        ..Default::default()
    };
    let r1 = prune_session(&src, &opts).unwrap();
    let out1 = std::fs::read_to_string(&r1.new_path).unwrap();
    let markers1 = out1.matches(MARKER_PREFIX).count();
    assert!(markers1 > 0);

    // Pruning the pruned session again must find nothing new: markers are never re-snipped.
    let r2 = prune_session(&r1.new_path, &opts).unwrap();
    assert_eq!(r2.pruned_count, 0, "second prune snips nothing");
    assert!(r2.sidecar_path.is_none(), "no new sidecar for a no-op prune");
    let out2 = std::fs::read_to_string(&r2.new_path).unwrap();
    assert_eq!(out2.matches(MARKER_PREFIX).count(), markers1, "no nested markers");
    // the originals are still retrievable from the FIRST sidecar
    let restored = retrieve(r1.sidecar_path.as_ref().unwrap(), "toolu_1").unwrap();
    assert_eq!(restored.as_str().unwrap(), big);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn pruned_session_reparses_via_claude_adapter_with_intact_turns() {
    // The in-repo resume-safety proxy: the pruned file must still parse through the Claude harness
    // adapter with the same turn structure (ids, roles, count) as the source.
    let dir = tmpdir();
    let big = "P".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions {
        keep_last: 0,
        revive: true,
        ..Default::default()
    };
    let r = prune_session(&src, &opts).unwrap();

    let orig = crate::harness::claude::parse_str("orig", &std::fs::read_to_string(&src).unwrap(), None);
    let pruned_text = std::fs::read_to_string(&r.new_path).unwrap();
    let pruned = crate::harness::claude::parse_str("pruned", &pruned_text, None);

    assert!(!pruned.messages.is_empty());
    assert_eq!(pruned.messages.len(), orig.messages.len(), "same message count");
    for (o, p) in orig.messages.iter().zip(&pruned.messages) {
        assert_eq!(o.role, p.role, "roles preserved in order");
        assert_eq!(o.id, p.id, "message uuids preserved");
    }
    // and a windowed prune re-parses cleanly too, opening on a user turn
    let opts = PruneOptions {
        window: Some(1), // tiny budget → keeps only the newest turn (with an overshoot warning)
        ..Default::default()
    };
    let src2 = window_fixture(&dir);
    let r2 = prune_session(&src2, &opts).unwrap();
    let tail = crate::harness::claude::parse_str("tail", &std::fs::read_to_string(&r2.new_path).unwrap(), None);
    assert!(!tail.messages.is_empty(), "windowed tail still parses into turns");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn dry_run_writes_nothing() {
    let dir = tmpdir();
    let big = "Z".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions {
        dry_run: true,
        keep_last: 2,
        ..Default::default()
    };
    let r = prune_session(&src, &opts).unwrap();
    assert!(r.pruned_count >= 1);
    assert!(!r.new_path.exists(), "dry run must not write the new session");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn keep_last_large_spares_everything() {
    let dir = tmpdir();
    let big = "Q".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions {
        keep_last: 100,
        ..Default::default()
    };
    let r = prune_session(&src, &opts).unwrap();
    assert_eq!(r.pruned_count, 0, "nothing is old enough to snip");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn declassify_snips_all_security_prose_keeps_benign() {
    // A safeguard classifier scores the WHOLE loaded context, so security-dense PROSE anywhere — old
    // OR recent — can downgrade a resumed seat. --declassify snips EVERY security-dense message (user
    // string + assistant text blocks) into the sidecar (recency does NOT protect it, unlike the
    // tool/thinking passes), while leaving benign turns verbatim.
    let dir = tmpdir();
    let sid = "22222222-2222-4222-8222-222222222222";
    let secret_user = "We have a cross-tenant auth bypass exploit; the credential exfil risk is high.";
    let secret_asst = "The vulnerability lets an attacker exfil credentials via the exploit chain.";
    let benign_user = "Great, unrelated. Remember: my favorite color is teal.";
    let recent_secret = "Recent turn: the security exploit vulnerability detail is also cleared.";
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","timestamp":"2026-07-01T00:00:00Z",
            "message":{"role":"user","content":secret_user}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","parentUuid":"u0","timestamp":"2026-07-01T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":secret_asst}]}}),
        // benign turn -> keep verbatim (0 security terms)
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u1","parentUuid":"a1","timestamp":"2026-07-01T00:00:02Z",
            "message":{"role":"user","content":benign_user}}),
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a2","parentUuid":"u1","timestamp":"2026-07-01T00:00:03Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"Noted, teal."}]}}),
        // RECENT but security-dense -> ALSO snipped (recency does not protect a trigger span)
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u2","parentUuid":"a2","timestamp":"2026-07-01T00:00:04Z",
            "message":{"role":"user","content":recent_secret}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();

    // keep_last default (25) would mark all turns "recent" — declassify must snip regardless.
    // Tokens are caller-supplied (cv ships no built-in list); supply the ones this session uses.
    let opts = PruneOptions {
        declassify: true,
        declassify_tokens: [
            "security",
            "exploit",
            "vulnerability",
            "cross-tenant",
            "auth bypass",
            "credential",
            "exfil",
            "attacker",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    let out = std::fs::read_to_string(&r.new_path).unwrap();

    // ALL security-dense prose is snipped: originals gone, markers present.
    assert!(!out.contains(secret_user), "security user prose must be snipped");
    assert!(!out.contains(secret_asst), "security assistant prose must be snipped");
    assert!(
        !out.contains(recent_secret),
        "RECENT security prose is also snipped (recency is no shield)"
    );
    assert!(out.contains("tool=declassified"), "a declassify marker must be present");
    assert!(r.pruned_count >= 3, "all three dense messages snipped");
    // benign turn is untouched.
    assert!(out.contains("my favorite color is teal"), "benign prose kept verbatim");
    // lossless: the snipped user text is retrievable from the sidecar.
    let sidecar = r.sidecar_path.clone().expect("sidecar written");
    let got = retrieve(&sidecar, "u0#declass").unwrap();
    assert_eq!(got.as_str(), Some(secret_user), "snipped prose retrievable verbatim");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn declassify_spares_benign_sessions_entirely() {
    // A session with < min_hits security terms per message must be left byte-untouched by declassify.
    let dir = tmpdir();
    let sid = "33333333-3333-4333-8333-333333333333";
    let lines = [
        serde_json::json!({"type":"user","sessionId":sid,"uuid":"u0","timestamp":"2026-07-01T00:00:00Z",
            "message":{"role":"user","content":"Let's talk about the security fix we shipped."}}), // 1 term only
        serde_json::json!({"type":"assistant","sessionId":sid,"uuid":"a1","parentUuid":"u0","timestamp":"2026-07-01T00:00:01Z",
            "message":{"role":"assistant","content":[{"type":"text","text":"Sounds good, all resolved."}]}}),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    let opts = PruneOptions {
        keep_last: 0,
        declassify: true,
        declassify_tokens: ["security", "exploit", "vulnerability", "credential", "exfil"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        ..Default::default()
    };
    let r = prune_session(&path, &opts).unwrap();
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert_eq!(
        r.pruned_count, 0,
        "a single security term (< min_hits=2) must not trip the snip"
    );
    assert!(
        out.contains("the security fix we shipped"),
        "benign-density prose kept verbatim"
    );
    std::fs::remove_dir_all(&dir).ok();
}
