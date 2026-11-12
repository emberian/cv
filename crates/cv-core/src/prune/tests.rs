use super::*;
use crate::json::{ReadInput, Validation};

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

fn assistant_usage(total: u64) -> Value {
    serde_json::json!({"type":"assistant",
        "message":{"usage":{"input_tokens":total,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}})
}

/// Read fixture values through prune's own decision pass (estimates included), as JSONL.
fn fixture_decisions(lines: &[Value]) -> Vec<Option<Decision>> {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let path = dir.join("decisions.jsonl");
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    let source = crate::json::JsonSource::capture(&path).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    let options = PruneOptions {
        revive: true,
        ..Default::default()
    };
    let mut tools = HashMap::new();
    let decisions = ranges::records(
        &file,
        Range {
            offset: 0,
            len: source.len(),
        },
    )
    .unwrap()
    .map(|row| {
        let (range, _) = row.unwrap();
        let read = source.read(ReadInput::Range(range.offset, range.len), Validation::Value, |v| {
            decide(v, range, &options, &mut tools, &mut PlanCheck::default())
        });
        Some(read.unwrap())
    })
    .collect();

    decisions
}

#[test]
fn window_cutoff_sizes_by_real_recorded_usage() {
    // 5 assistant turns, cumulative usage 100k..500k (one compaction segment) — 100k per turn.
    let lines: Vec<Value> = (1..=5).map(|k| assistant_usage(k * 100_000)).collect();
    let parsed = fixture_decisions(&lines);
    // budget 250k → the LARGEST tail whose real size stays ≤ 250k is turns 3,4 (200k real).
    // (Keeping turn 2 as well would be 300k — over budget; the contract is ≤, never ≥.)
    assert_eq!(usage_window_cutoff(&parsed, 250_000), Some((3, 200_000, false)));
    // budget beyond the whole session → keep everything.
    assert_eq!(usage_window_cutoff(&parsed, 9_000_000), Some((0, 500_000, false)));
    // no usage records → None (caller falls back to a byte estimate).
    assert_eq!(
        usage_window_cutoff(&fixture_decisions(&[serde_json::json!({"type":"user"})]), 100),
        None
    );
}

#[test]
fn window_cutoff_keeps_single_huge_turn_but_flags_overshoot() {
    let lines: Vec<Value> = (1..=5).map(|k| assistant_usage(k * 100_000)).collect();
    // Even the newest turn alone (100k) busts a 50k budget: keep that one turn, flag it — the
    // caller must warn that the ≤-budget contract could not be honored.
    assert_eq!(
        usage_window_cutoff(&fixture_decisions(&lines), 50_000),
        Some((4, 100_000, true))
    );
}

#[test]
fn window_cutoff_skips_sidechain_usage() {
    // A sub-agent (isSidechain) turn carries a HUGE usage from its own context window. It must not
    // poison the main thread's sizing: with it ignored, the tail is sized purely by the main turns.
    let mut side = assistant_usage(950_000);
    side["isSidechain"] = Value::Bool(true);
    let lines = [assistant_usage(100_000), side, assistant_usage(200_000)];
    // Main turns: 0 (100k) and 1 (200k → delta 100k). Budget 150k → keep from main turn 1.
    assert_eq!(
        usage_window_cutoff(&fixture_decisions(&lines), 150_000),
        Some((1, 100_000, false))
    );
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
    assert_eq!(
        usage_window_cutoff(&fixture_decisions(&lines), 150_000),
        Some((2, 100_000, false))
    );
}

#[test]
fn prunes_old_payloads_into_new_session_keeping_recent() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
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
    let r = prune_session(crate::Harness::Claude, &src, &opts).unwrap();

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
}

#[test]
fn drop_mode_writes_no_sidecar_and_never_advertises_retrieve() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
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
    let r = prune_session(crate::Harness::Claude, &src, &opts).unwrap();
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
}

#[test]
fn drop_thinking_omits_old_reasoning_and_relinks_emptied_records() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let big = "R".repeat(5000);
    let sid = "22222222-2222-4222-8222-222222222222";
    // Old thinking-only record a0 (outside keep_last) and a recent one a2 (within it).
    let line = |uuid: &str, parent: Option<&str>, role: &str, content: serde_json::Value| {
        serde_json::json!({"type":role,"sessionId":sid,"uuid":uuid,"parentUuid":parent,
            "timestamp":"2026-06-18T00:00:00Z","message":{"role":role,"content":content}})
    };
    let think = |t: &str| serde_json::json!([{"type":"thinking","thinking":t,"signature":"sig"}]);
    let lines = [
        line("u0", None, "user", serde_json::json!("start")),
        line("a0", Some("u0"), "assistant", think(&big)),
        line(
            "t0",
            Some("a0"),
            "assistant",
            serde_json::json!([{"type":"text","text":"done"}]),
        ),
        line("u1", Some("t0"), "user", serde_json::json!("more")),
        line("a1", Some("u1"), "assistant", serde_json::json!("ok")),
        line("u2", Some("a1"), "user", serde_json::json!("again")),
        line("a2", Some("u2"), "assistant", think(&big)),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    let input = lines.iter().map(|l| l.to_string() + "\n").collect::<String>();
    std::fs::write(&path, &input).unwrap();

    let opts = PruneOptions {
        min_size: 1024,
        keep_last: 2,
        thinking: true,
        ..Default::default()
    };
    let r = prune_session(crate::Harness::Claude, &path, &opts).unwrap();
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert!(out.len() < input.len(), "dropping thinking never grows the transcript");
    assert!(
        !out.contains("[PRUNED"),
        "no inline marker: it would give the model nothing"
    );
    // The emptied record is gone; its child points at its parent and the chain still resumes.
    assert!(!out.contains("\"uuid\":\"a0\""));
    let t0: Value = serde_json::from_str(out.lines().find(|l| l.contains("\"uuid\":\"t0\"")).unwrap()).unwrap();
    assert_eq!(t0["parentUuid"], "u0");
    crate::harness::claude::emit::verify_claude_resumable(&r.new_path, None).unwrap();
    // Recent thinking (a2, within keep_last=2) is kept verbatim.
    let a2 = out.lines().find(|l| l.contains("\"uuid\":\"a2\"")).unwrap();
    assert!(
        a2.contains(&big) && a2.contains("\"thinking\""),
        "recent thinking kept verbatim"
    );
    // The dropped block is retrievable by record uuid and block index.
    let restored = retrieve(r.sidecar_path.as_ref().unwrap(), "a0#think0").unwrap();
    assert_eq!(restored.get("thinking").and_then(|v| v.as_str()), Some(big.as_str()));
}

#[test]
fn revive_rewrites_stale_usage_below_loaded_content() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
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
    let r = prune_session(crate::Harness::Claude, &path, &opts).unwrap();

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
        honest < 25_000,
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
}

#[test]
fn usage_total_reads_the_last_request_iteration_like_claude_code() {
    // Claude Code ≥ 2.1.277 sizes context from `usage.iterations` (the last non-advisor/compaction
    // request) — the top-level counters are only a fallback. cv must see the same number.
    let u = |v: Value| {
        crate::harness::claude::record::usage_context((&v).into())
            .unwrap()
            .unwrap()
    };
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
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
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
    let r = prune_session(crate::Harness::Claude, &path, &opts).unwrap();
    assert_eq!(r.usage_rewritten, 1);
    assert_eq!(
        r.revive_old_tokens,
        Some(975_298),
        "the stale figure is read from the request iteration, not the (already small) top level"
    );
    let honest = r.revive_tokens.expect("honest figure written");
    assert!(
        honest < 25_000,
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
    assert_eq!(
        crate::harness::claude::record::usage_context(u.into()).unwrap(),
        Some(honest)
    );

    // The produced file's own reader needs the stash: a later `--window` prune of the revived
    // session sizes its tail from `_cv_orig_ctx`, not the pinned usage. Without it the same
    // file reads as a tail that fits, and the window would hand back a session that cannot resume.
    let window = |source: &Path, to: &str| {
        let opts = PruneOptions {
            window: Some(100_000),
            new_id: Some(to.into()),
            dry_run: true,
            ..Default::default()
        };
        prune_session(crate::Harness::Claude, source, &opts)
            .unwrap()
            .window_real_tokens
    };
    assert_eq!(
        window(&r.new_path, "55555555-5555-4555-8555-555555555555"),
        Some(975_298)
    );
    let stripped = dir.join("66666666-6666-4666-8666-666666666666.jsonl");
    let without: String = out
        .lines()
        .map(|l| {
            let mut v: Value = serde_json::from_str(l).unwrap();
            v.as_object_mut().unwrap().remove("_cv_orig_ctx");
            v.to_string() + "\n"
        })
        .collect();
    std::fs::write(&stripped, without).unwrap();
    assert_eq!(window(&stripped, "77777777-7777-4777-8777-777777777777"), Some(honest));
}

#[test]
fn synthetic_notices_are_not_turns() {
    // Claude Code's client-side "Prompt is too long" rows are `assistant` records with
    // `model: "<synthetic>"`. They ride along as records but never count as turns.
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let sid = "55555555-5555-4555-8555-555555555555";
    let mk = |ty: &str, uuid: &str, content: Value| {
        serde_json::json!({"type":ty,"sessionId":sid,"uuid":uuid,"timestamp":"2026-09-19T00:00:00Z",
            "message":{"role":ty,"content":content}})
    };
    let mut synth = mk(
        "assistant",
        "e1",
        serde_json::json!([{"type":"text","text":"Prompt is too long"}]),
    );
    synth["message"]["model"] = "<synthetic>".into();
    synth["isApiErrorMessage"] = true.into();
    let lines = [
        mk("user", "u0", "first".into()),
        synth.clone(),
        mk("assistant", "a1", serde_json::json!([{"type":"text","text":"reply"}])),
        mk("user", "u1", "second".into()),
        synth,
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    // turns are u0, a1, u1 — the notices don't count; `--range 1-` keeps a1 + u1 (and the notices,
    // which travel with the turns they precede)
    let opts = PruneOptions {
        keep_range: Some((1, None)),
        ..Default::default()
    };
    let r = prune_session(crate::Harness::Claude, &path, &opts).unwrap();
    assert_eq!(r.dropped_turns, 1);
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert!(!out.contains("\"uuid\":\"u0\""));
    assert!(out.contains("\"uuid\":\"a1\"") && out.contains("\"uuid\":\"u1\""));
    assert_eq!(
        out.matches("Prompt is too long").count(),
        2,
        "notices are kept as records, just not counted"
    );
    let lines: Vec<Value> = out.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let turns = fixture_decisions(&lines)
        .iter()
        .flatten()
        .filter(|d| d.is_main_turn())
        .count();
    assert_eq!(turns, 2);
}

#[test]
fn window_keeps_last_singleton_records_and_restamps_both_id_spellings() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let sid = "66666666-6666-4666-8666-666666666666";
    let turn = |ty: &str, uuid: &str, text: &str| {
        serde_json::json!({"type":ty,"sessionId":sid,"session_id":sid,"uuid":uuid,"timestamp":"2026-09-19T00:00:00Z",
            "message":{"role":ty,"content":text}})
    };
    let lines = [
        serde_json::json!({"type":"custom-title","customTitle":"My rename","sessionId":sid}),
        serde_json::json!({"type":"agent-name","agentName":"Fixing prune","sessionId":sid}),
        serde_json::json!({"type":"ai-title","aiTitle":"old auto title","sessionId":sid}),
        turn("user", "u0", "first"),
        turn("assistant", "a0", "r0"),
        serde_json::json!({"type":"ai-title","aiTitle":"newer auto title","sessionId":sid}),
        turn("user", "u1", "second"),
        turn("assistant", "a1", "r1"),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    let opts = PruneOptions {
        keep_range: Some((2, None)),
        ..Default::default()
    };
    let r = prune_session(crate::Harness::Claude, &path, &opts).unwrap();
    assert_eq!(r.dropped_turns, 2);
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert!(!out.contains("\"uuid\":\"u0\""));
    assert_eq!(
        out.matches("\"type\":\"custom-title\"").count(),
        1,
        "the /rename title survives a head drop"
    );
    assert_eq!(out.matches("\"type\":\"agent-name\"").count(), 1);
    assert_eq!(
        out.matches("\"type\":\"ai-title\"").count(),
        1,
        "only the LAST ai-title is kept"
    );
    assert!(out.contains("newer auto title") && !out.contains("old auto title"));
    // both id spellings restamped — the source id is gone entirely
    assert!(!out.contains(sid));
    assert_eq!(out.matches(&format!("\"session_id\":\"{}\"", r.new_id)).count(), 2);
}

#[test]
fn thinking_flatten_snips_signature_only_blocks() {
    // Fable-era transcripts keep `thinking: ""` + a ~600-byte signature per turn. On the wire those
    // blocks still cost hundreds of tokens each, so `--thinking` must not skip them for being small.
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let sid = "77777777-7777-4777-8777-777777777777";
    let sig = "C".repeat(600);
    let mk = |uuid: &str, ty: &str, content: Value| {
        serde_json::json!({"type":ty,"sessionId":sid,"uuid":uuid,"timestamp":"2026-09-19T00:00:00Z",
            "message":{"role":ty,"content":content}})
    };
    let think = |reply: &str| serde_json::json!([{"type":"thinking","thinking":"","signature":sig},{"type":"text","text":reply}]);
    let lines = [
        mk("u0", "user", "hi".into()),
        mk("a0", "assistant", think("old reply")),
        mk("u1", "user", "again".into()),
        mk("a1", "assistant", think("recent reply")),
    ];
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    let opts = PruneOptions {
        thinking: true,
        keep_last: 2,
        ..Default::default()
    };
    let r = prune_session(crate::Harness::Claude, &path, &opts).unwrap();
    assert_eq!(
        r.pruned_count, 1,
        "the OLD signature-only block is snipped; the recent one (keep-last) stays"
    );
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    let a0: Value = serde_json::from_str(out.lines().find(|l| l.contains("\"uuid\":\"a0\"")).unwrap()).unwrap();
    assert_eq!(
        a0["message"]["content"],
        serde_json::json!([{"type":"text","text":"old reply"}])
    );
    let a1 = out.lines().find(|l| l.contains("\"uuid\":\"a1\"")).unwrap();
    assert!(a1.contains(&sig));
}

#[test]
fn rendered_attachments_count_toward_byte_estimates() {
    // A model-visible attachment (`rendered[]`) is prompt text; the honest figure must include it.
    let big = "x".repeat(35_000);
    let lines = [
        serde_json::json!({"type":"user","uuid":"u0","message":{"role":"user","content":"hi"}}),
        serde_json::json!({"type":"attachment","uuid":"t0","attachment":{"type":"instructions"},"rendered":[{"content":big}]}),
        serde_json::json!({"type":"attachment","uuid":"t1","attachment":{"type":"hook_success"}}),
        serde_json::json!({"type":"assistant","uuid":"a0","message":{"role":"assistant","content":[{"type":"text","text":"ok"}],
            "usage":{"input_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}),
    ];
    let parsed = fixture_decisions(&lines);
    let out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let honest = honest_window_tokens(
        &parsed,
        &out.iter().map(|l| l.contains(MARKER_PREFIX)).collect::<Vec<_>>(),
        0,
        true,
    );
    assert!(
        honest >= 10_000,
        "35k bytes of rendered reminder ≈ 10k tokens must be counted (got {honest})"
    );
    assert_eq!(
        parsed[2].as_ref().unwrap().est.total(),
        0,
        "an unrendered attachment costs nothing"
    );
}

#[test]
fn revive_honors_recorded_delta_evidence_over_low_byte_estimate() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let sid = "44444444-4444-4444-8444-444444444444";
    // No boundary, nothing snipped in the second span: the recorded usage DELTA (30k between a0 and
    // a1) is hard evidence that span really costs 30k, even though its byte estimate is tiny (e.g.
    // dense code that tokenizes far above the byte estimate). Revive must never write a figure below that
    // evidence — the old byte-only estimator did, and let resumes through that blew the real limit.
    let usage =
        |n: u64| serde_json::json!({"input_tokens":n,"cache_read_input_tokens":0,"cache_creation_input_tokens":0});
    let big = "B".repeat(100_000);
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
    let r = prune_session(crate::Harness::Claude, &path, &opts).unwrap();
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
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let src = window_fixture(&dir);
    // 120k budget: each pair costs 100k real, so only the newest pair fits (≤ contract).
    let opts = PruneOptions {
        window: Some(120_000),
        ..Default::default()
    };
    let r = prune_session(crate::Harness::Claude, &src, &opts).unwrap();

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
}

#[test]
fn window_overshoot_keeps_the_huge_turn_and_warns() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
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
    let r = prune_session(crate::Harness::Claude, &path, &opts).unwrap();
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
}

#[test]
fn window_and_revive_ignore_sidechain_turns() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
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
    let r = prune_session(crate::Harness::Claude, &path, &opts).unwrap();

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
}

#[test]
fn sidecar_ids_are_unique_and_round_trip_arrays_and_objects() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
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
    let r = prune_session(crate::Harness::Claude, &path, &opts).unwrap();
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
}

#[test]
fn retrieve_errors_on_duplicate_ids_instead_of_guessing() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
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
}

#[test]
fn double_prune_is_idempotent() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let big = "D".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions {
        keep_last: 0,
        ..Default::default()
    };
    let r1 = prune_session(crate::Harness::Claude, &src, &opts).unwrap();
    let out1 = std::fs::read_to_string(&r1.new_path).unwrap();
    let markers1 = out1.matches(MARKER_PREFIX).count();
    assert!(markers1 > 0);

    // Pruning the pruned session again must find nothing new: markers are never re-snipped.
    let r2 = prune_session(crate::Harness::Claude, &r1.new_path, &opts).unwrap();
    assert_eq!(r2.pruned_count, 0, "second prune snips nothing");
    assert!(r2.sidecar_path.is_none(), "no new sidecar for a no-op prune");
    let out2 = std::fs::read_to_string(&r2.new_path).unwrap();
    assert_eq!(out2.matches(MARKER_PREFIX).count(), markers1, "no nested markers");
    // the originals are still retrievable from the FIRST sidecar
    let restored = retrieve(r1.sidecar_path.as_ref().unwrap(), "toolu_1").unwrap();
    assert_eq!(restored.as_str().unwrap(), big);
}

#[test]
fn pruned_session_reparses_via_claude_adapter_with_intact_turns() {
    // The in-repo resume-safety proxy: the pruned file must still parse through the Claude harness
    // adapter with the same turn structure (ids, roles, count) as the source.
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let big = "P".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions {
        keep_last: 0,
        revive: true,
        ..Default::default()
    };
    let r = prune_session(crate::Harness::Claude, &src, &opts).unwrap();

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
    let r2 = prune_session(crate::Harness::Claude, &src2, &opts).unwrap();
    let tail = crate::harness::claude::parse_str("tail", &std::fs::read_to_string(&r2.new_path).unwrap(), None);
    assert!(!tail.messages.is_empty(), "windowed tail still parses into turns");
}

#[test]
fn dry_run_writes_nothing() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let big = "Z".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions {
        dry_run: true,
        keep_last: 2,
        ..Default::default()
    };
    let r = prune_session(crate::Harness::Claude, &src, &opts).unwrap();
    assert!(r.pruned_count >= 1);
    assert!(!r.new_path.exists(), "dry run must not write the new session");
}

#[test]
fn keep_last_large_spares_everything() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let big = "Q".repeat(5000);
    let src = fixture(&dir, &big);
    let opts = PruneOptions {
        keep_last: 100,
        ..Default::default()
    };
    let r = prune_session(crate::Harness::Claude, &src, &opts).unwrap();
    assert_eq!(r.pruned_count, 0, "nothing is old enough to snip");
}

#[test]
fn declassify_snips_all_security_prose_keeps_benign() {
    // A safeguard classifier scores the WHOLE loaded context, so security-dense PROSE anywhere — old
    // OR recent — can downgrade a resumed seat. --declassify snips EVERY security-dense message (user
    // string + assistant text blocks) into the sidecar (recency does NOT protect it, unlike the
    // tool/thinking passes), while leaving benign turns verbatim.
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
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
    let r = prune_session(crate::Harness::Claude, &path, &opts).unwrap();
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
}

#[test]
fn declassify_spares_benign_sessions_entirely() {
    // A session with < min_hits security terms per message must be left byte-untouched by declassify.
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
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
    let r = prune_session(crate::Harness::Claude, &path, &opts).unwrap();
    let out = std::fs::read_to_string(&r.new_path).unwrap();
    assert_eq!(
        r.pruned_count, 0,
        "a single security term (< min_hits=2) must not trip the snip"
    );
    assert!(
        out.contains("the security fix we shipped"),
        "benign-density prose kept verbatim"
    );
}

#[test]
fn decision_roster_preserves_selection_and_revive_evidence() {
    let rows = vec![
        serde_json::json!({"type":"user","message":{"content":"opening prompt"}}),
        serde_json::json!({"type":"assistant","message":{"content":[{"type":"thinking","thinking":"reasoning","signature":"sig"}],"usage":{"input_tokens":12,"cache_read_input_tokens":5,"iterations":[{"type":"message","input_tokens":30,"cache_read_input_tokens":4,"cache_creation_input_tokens":1,"output_tokens":2}]}}}),
        serde_json::json!({"type":"assistant","isSidechain":true,"message":{"content":"other context","usage":{"input_tokens":999999}}}),
        serde_json::json!({"type":"system","subtype":"compact_boundary"}),
        serde_json::json!({"type":"attachment","attachment":{"type":"memory","rendered":["system reminder"]}}),
        serde_json::json!({"type":"assistant","message":{"model":"<synthetic>","content":"Prompt too long"}}),
        serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","content":"[PRUNED id=t]"}]}}),
        serde_json::json!({"type":"assistant","_cv_orig_ctx":120,"message":{"content":"answer","usage":{"input_tokens":100}}}),
    ];
    // Recorded from the owned-row and decision-roster readings, which agreed, before both were
    // replaced by the typed facts.
    let facts = fixture_decisions(&rows);
    for (budget, kept) in [
        (0, (2, 4, Some(120), true)),
        (1, (2, 4, Some(120), true)),
        (30, (2, 4, Some(120), true)),
        (100, (2, 4, Some(120), true)),
        (200, (0, 4, Some(155), false)),
        (1000, (0, 4, Some(155), false)),
    ] {
        let options = PruneOptions {
            window: Some(budget),
            ..Default::default()
        };
        assert_eq!(select_kept_turns(&facts, &options, 4), kept, "budget {budget}");
    }
    let pruned: Vec<_> = rows.iter().map(|v| v.to_string().contains(MARKER_PREFIX)).collect();
    for (window, honest) in [(0, 89), (4, 54)] {
        for trusted in [true, false] {
            assert_eq!(honest_window_tokens(&facts, &pruned, window, trusted), honest);
        }
    }
}

#[test]
fn sidecar_preparation_failure_preserves_existing_transcript() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let src = fixture(&dir, &"payload".repeat(1024));
    let new_id = "22222222-2222-4222-8222-222222222222";
    let transcript = dir.join(format!("{new_id}.jsonl"));
    std::fs::write(&transcript, "prior destination").unwrap();
    std::fs::create_dir(dir.join(format!("{new_id}.flat.jsonl"))).unwrap();
    let options = PruneOptions {
        new_id: Some(new_id.into()),
        keep_last: 0,
        force: true,
        ..Default::default()
    };
    assert!(prune_session(crate::Harness::Claude, &src, &options).is_err());
    assert_eq!(std::fs::read_to_string(transcript).unwrap(), "prior destination");
    assert!(!std::fs::read_dir(&dir).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".cv-prune-")));
}

/// Sidecar and transcript are one publication: under `--force` the sidecar replaces its name,
/// the transcript's claim then fails (a directory holds its name), and the replaced sidecar is
/// put back rather than lost.
#[test]
fn a_failed_transcript_claim_restores_the_replaced_sidecar() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let src = fixture(&dir, &"payload".repeat(1024));
    let new_id = "22222222-2222-4222-8222-222222222222";
    let sidecar = dir.join(format!("{new_id}.flat.jsonl"));
    std::fs::write(&sidecar, "prior sidecar").unwrap();
    std::fs::create_dir(dir.join(format!("{new_id}.jsonl"))).unwrap();
    let options = PruneOptions {
        new_id: Some(new_id.into()),
        keep_last: 0,
        force: true,
        ..Default::default()
    };
    let error = format!(
        "{:#}",
        prune_session(crate::Harness::Claude, &src, &options).unwrap_err()
    );
    assert!(error.contains("is a directory"), "{error}");
    assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "prior sidecar");
    assert!(!std::fs::read_dir(&dir)
        .unwrap()
        .any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".cv-")));
}

#[test]
fn renamed_source_cannot_be_its_own_prune_destination() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let src = fixture(&dir, &"payload".repeat(1024));
    let new_id = "22222222-2222-4222-8222-222222222222";
    let renamed = dir.join(format!("{new_id}.jsonl"));
    std::fs::rename(src, &renamed).unwrap();
    let original = std::fs::read(&renamed).unwrap();
    let options = PruneOptions {
        new_id: Some(new_id.into()),
        keep_last: 0,
        ..Default::default()
    };
    assert!(prune_session(crate::Harness::Claude, &renamed, &options).is_err());
    assert_eq!(std::fs::read(renamed).unwrap(), original);
}

#[test]
fn giant_ignored_row_preserves_raw_bytes_and_recorded_usage() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let sid = "11111111-1111-4111-8111-111111111111";
    let new_id = "22222222-2222-4222-8222-222222222222";
    let line = format!(" {{ \"sessionId\":\"{sid}\", \"type\":\"user\", \"message\":{{\"content\":\"prompt\"}}, \"unrelated\":\"{}[PRUNED id=fake]{}\" }}", "x".repeat(1024 * 1024), "x".repeat(1024 * 1024));
    let usage = serde_json::json!({"type":"assistant","sessionId":sid,"message":{"content":"answer","usage":{"input_tokens":100000000}}});
    let src = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&src, format!("{line}\n{usage}\n")).unwrap();
    let options = PruneOptions {
        new_id: Some(new_id.into()),
        revive: true,
        keep_last: 0,
        ..Default::default()
    };
    let result = prune_session(crate::Harness::Claude, &src, &options).unwrap();
    let output = std::fs::read_to_string(result.new_path).unwrap();
    assert_eq!(
        output.lines().next().unwrap(),
        crate::restamp_line(&line, new_id).unwrap()
    );
    assert_eq!(result.pruned_count, 0);
    // A marker-shaped string outside the model-visible content is no evidence: nothing was pruned,
    // so the recorded prompt stands and nothing is rewritten.
    assert_eq!(result.usage_rewritten, 0);
}

#[test]
fn marker_writer_detects_every_chunk_split_with_bounded_tail() {
    let bytes = format!("prefix {MARKER_PREFIX}fake] suffix").into_bytes();
    for split in 0..=bytes.len() {
        let mut output = Vec::new();
        let mut writer = MarkerWriter::new(&mut output);
        writer.write_all(&bytes[..split]).unwrap();
        writer.write_all(&bytes[split..]).unwrap();
        assert!(writer.found);
        assert!(writer.tail.len() < MARKER_PREFIX.len());
        assert_eq!(output, bytes);
    }
}

/// An existing destination, a symlink or hard link included, is refused without a byte written;
/// `force` replaces the name by rename, leaving the old inode (and what it links to) intact.
#[cfg(unix)]
#[test]
fn existing_output_is_refused_and_force_replaces_the_name_not_the_inode() {
    use std::os::unix::fs::{symlink, MetadataExt};
    for symbolic in [true, false] {
        let world = crate::test_world("prune");
        let dir = world.proj.clone();
        let source = fixture(&dir, &"payload".repeat(1024));
        let new_id = "22222222-2222-4222-8222-222222222222";
        let target = dir.join("existing-target");
        let destination = dir.join(format!("{new_id}.jsonl"));
        std::fs::write(&target, "before").unwrap();
        if symbolic {
            symlink(&target, &destination).unwrap();
        } else {
            std::fs::hard_link(&target, &destination).unwrap();
        }
        let inode = std::fs::metadata(&target).unwrap().ino();
        let mut options = PruneOptions {
            new_id: Some(new_id.into()),
            keep_last: 0,
            ..Default::default()
        };
        let error = prune_session(crate::Harness::Claude, &source, &options).unwrap_err();
        assert!(error.downcast_ref::<crate::publish::Exists>().is_some(), "{error:#}");
        assert!(!dir.join(format!("{new_id}.flat.jsonl")).exists());
        options.force = true;
        prune_session(crate::Harness::Claude, &source, &options).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
        assert_eq!(std::fs::metadata(&target).unwrap().ino(), inode);
        assert!(!std::fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_to_string(&destination).unwrap().contains(MARKER_PREFIX));
    }
}

#[cfg(unix)]
#[test]
fn hardlinked_source_is_rejected_before_truncation() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let source = fixture(&dir, &"payload".repeat(1024));
    let original = std::fs::read(&source).unwrap();
    let new_id = "22222222-2222-4222-8222-222222222222";
    std::fs::hard_link(&source, dir.join(format!("{new_id}.jsonl"))).unwrap();
    let options = PruneOptions {
        new_id: Some(new_id.into()),
        keep_last: 0,
        ..Default::default()
    };
    assert!(prune_session(crate::Harness::Claude, &source, &options).is_err());
    assert_eq!(std::fs::read(source).unwrap(), original);
}

#[test]
fn source_string_mutation_matches_frozen_output_sidecars_and_estimate() {
    for drop in [false, true] {
        let world = cv_testkit::World::new("prune-source-string-mutation");
        let text = format!("{}\n{}\r\n🌙\\\"\n", "x".repeat(1024 * 1024), "a".repeat(70000));
        let owned = serde_json::json!({"type":"user","sessionId":"old","session_id":"old2","uuid":"line","message":{"content":[
            {"type":"tool_result","tool_use_id":"t","content":text,"extra":1.50},
            {"type":"text","text":"keep"},
            {"type":"tool_result","content":"small"},
            {"type":"tool_result","content":"two\nlines\n"},
            {"type":"image","source":{"data":"small"}}
        ]},"parentUuid":"p"});
        let raw = serde_json::to_string_pretty(&owned).unwrap();
        let raw = format!("{{\"sessionId\":\"shadow\",{}", raw.strip_prefix('{').unwrap());
        let path = world.base.join("input.jsonl");
        std::fs::write(&path, &raw).unwrap();
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let range = Range {
            offset: 0,
            len: raw.len() as u64,
        };
        source
            .read(ReadInput::Range(0, range.len), Validation::Value, |_| Ok(()))
            .unwrap();
        let opts = PruneOptions {
            min_size: 4,
            drop,
            revive: true,
            ..Default::default()
        };
        let plan = source_user_prune(&source, range, &opts, "new", 0, &HashMap::new())
            .unwrap()
            .unwrap();
        assert_frozen_plan(&format!("string-{drop}"), Some(&plan));
    }
}

#[test]
fn source_user_mutation_matches_frozen_rich_payload_and_mirror() {
    let cases = [
        serde_json::json!({"message":{"content":[{"type":"tool_result","content":[
            {"type":"text","text":"first\n"},{"type":"text","text":""},
            {"type":"image","source":{"data":"x".repeat(1024*1024)}},
            {"type":"text","text":"tail"},{"type":"image"}
        ]}]},"toolUseResult":{"file":{"base64":null},"text":"mirror"}}),
        serde_json::json!({"message":{"content":[{"type":"tool_result","tool_use_id":"absent"},
            {"type":"tool_result","content":[]},{"type":"tool_result","content":{}},
            {"type":"tool_result","content":[{"unknown":"payload"}]}]},
            "toolUseResult":{"type":"image","body":"mirror"}}),
        serde_json::json!({"message":{"content":"prompt\n"},"toolUseResult":"mirror\nlines\n"}),
        serde_json::json!({"message":{"content":[{"type":"tool_result","content":"[PRUNED id=already]"}]},
            "toolUseResult":"[PRUNED id=mirror]"}),
    ];
    for (case, original) in cases.into_iter().enumerate() {
        for drop in [false, true] {
            let world = cv_testkit::World::new("source-rich-user-prune");
            let mut owned = original.clone();
            owned["type"] = Value::String("user".into());
            owned["sessionId"] = Value::String("old".into());
            let raw = serde_json::to_string_pretty(&owned).unwrap();
            let path = world.base.join("input.jsonl");
            std::fs::write(&path, &raw).unwrap();
            let source = crate::json::JsonSource::capture(&path).unwrap();
            let range = Range {
                offset: 0,
                len: raw.len() as u64,
            };
            source
                .read(ReadInput::Range(0, range.len), Validation::Value, |_| Ok(()))
                .unwrap();
            let opts = PruneOptions {
                min_size: 4,
                drop,
                revive: true,
                ..Default::default()
            };
            let plan = source_user_prune(&source, range, &opts, "new", 0, &HashMap::new()).unwrap();
            assert_frozen_plan(&format!("rich-{case}-{drop}"), plan.as_ref());
        }
    }
}

#[test]
fn source_reroot_preserves_owned_stamp_order_including_duplicate_ids() {
    for ids in [
        r#""sessionId":"old","session_id":"other","#,
        r#""sessionId":"shadow","sessionId":"old","#,
    ] {
        let world = cv_testkit::World::new("source-prune-reroot");
        let raw=format!("{{{ids}\"type\":\"user\",\"parentUuid\":\"parent\",\"ignored\":\"{}\",\"message\":{{\"content\":\"[PRUNED id=existing]\"}}}}","x".repeat(1024*1024));
        let path = world.base.join("input.jsonl");
        std::fs::write(&path, &raw).unwrap();
        let source = crate::json::JsonSource::capture(&path).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let mut selected = crate::derive::Select::read(
            &source,
            &file,
            Range {
                offset: 0,
                len: raw.len() as u64,
            },
            true,
            false,
            &crate::harness::claude::IDENTITY,
        )
        .unwrap();
        selected.root = true;
        let expected = crate::restamp_line(&raw, "new").unwrap_or(raw);
        let mut expected: Value = serde_json::from_str(&expected).unwrap();
        expected["parentUuid"] = Value::Null;
        let mut actual = Vec::new();
        crate::derive::derive(
            &source,
            &file,
            std::iter::once(Ok(selected)),
            &crate::derive::Rewrite {
                session_id: "new",
                standalone: false,
                identity: crate::harness::claude::IDENTITY,
            },
            &mut actual,
        )
        .unwrap();
        assert_eq!(actual, format!("{expected}\n").as_bytes());
    }
}

#[test]
fn retrieve_projection_acceptance_and_error_semantics_are_the_recorded_ones() {
    let world = crate::test_world("retrieve-projection-oracle");
    let path = world.base.join("session.flat.jsonl");
    let row = |input: &str, content: &str, tail: &str| {
        format!(
            r#"{{"id":"wanted","slot":"content","name":"Read","input":{input},"content":{content},"size":1,"line_count":1,"kind":"text"{tail}}}"#
        )
    };
    let valid = row("null", r#"{"actual":["雪",true,null]}"#, "");
    let mut cases = vec![
        valid.clone(),
        format!("\r\n\u{2003}\nnot JSON\n{valid}\r\n"),
        format!("\u{2003}{valid}"),
        row("null", "null", ""),
        row("null", r#""actual""#, r#", "unknown":"\ud800", "number":1e400"#),
        row("null", r#""actual""#, r#", "unknown":null, "unknown":"\ud800"#),
        row("null", r#""actual""#, r#", "id":"wanted""#),
        row("null", r#""actual""#, r#", "\u0069d":"wanted""#),
        row("null", r#""actual""#, r#", "\ud800":null"#),
        row("1e400", r#""actual""#, ""),
        row("null", r#""\ud800""#, ""),
        row("null", "1e400", ""),
        valid.replace(r#""size":1"#, r#""size":1.0"#),
        valid.replace(r#""line_count":1"#, r#""line_count":-1"#),
        valid.replace(r#""size":1"#, r#""size":18446744073709551616"#),
        valid.replace(r#""size":1"#, r#""size":1e400"#),
        valid.replace(r#""name":"Read""#, r#""name":null"#),
        valid.replace(r#""input":null,""#, ""),
        r#"["wanted","content","Read",null,{"actual":"🌙"},1,1,"text"]"#.into(),
        r#"["wanted","content","Read",null,"actual",1,1,"text",null]"#.into(),
        r#"["wanted","content","Read",null,"actual",1,1]"#.into(),
        format!("{valid}\n{valid}\n{valid}"),
    ];
    for depth in 124..=130 {
        let nested = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
        cases.push(row(&nested, "null", ""));
        cases.push(row("null", "null", &format!(",\"unknown\":{nested}")));
    }
    for integer in [
        "-0",
        "0.0",
        "1e0",
        "18446744073709551615",
        "18446744073709551616",
        "true",
        "null",
        "-1",
    ] {
        cases.push(valid.replace(r#""size":1"#, &format!("\"size\":{integer}")));
        cases.push(valid.replace(r#""line_count":1"#, &format!("\"line_count\":{integer}")));
    }
    for key in ["id", "slot", "name", "input", "content", "size", "line_count", "kind"] {
        let mut missing: Value = serde_json::from_str(&valid).unwrap();
        missing.as_object_mut().unwrap().remove(key);
        cases.push(missing.to_string());
    }
    let shown = |result: &Result<Value>| match result {
        Ok(value) => serde_json::json!({ "ok": value }),
        Err(error) => serde_json::json!({ "err": format!("{error:#}").replace(path.to_str().unwrap(), "<path>") }),
    };
    let mut frozen = crate::emit::frozen::Frozen::new("prune-retrieve");
    for (i, case) in cases.into_iter().enumerate() {
        std::fs::write(&path, case).unwrap();
        for id in ["wanted", "absent"] {
            let actual = retrieve(&path, id);
            frozen.insert(format!("{i:03} {id}"), shown(&actual));
        }
    }
    let mut bytes = valid.into_bytes();
    bytes.extend_from_slice(b"\nmalformed\xff");
    std::fs::write(&path, bytes).unwrap();
    frozen.insert("invalid-utf8-tail", shown(&retrieve(&path, "wanted")));
    frozen.check().unwrap();
}

#[test]
fn retrieve_bounds_available_ids_and_rejects_captured_source_changes() {
    let world = crate::test_world("retrieve-source-guard");
    let path = world.base.join("session.flat.jsonl");
    let mut out = std::fs::File::create(&path).unwrap();
    for i in 0..40 {
        writeln!(
            out,
            "{}",
            sidecar_line(&format!("id-{i}"), "tool", &Value::Null, "actual")
        )
        .unwrap();
    }
    drop(out);
    // The message names the first twenty ids, no more.
    assert_eq!(
        retrieve(&path, "absent").unwrap_err().to_string(),
        format!(
            "id \"absent\" not found in sidecar. available: {}",
            (0..20).map(|i| format!("id-{i}")).collect::<Vec<_>>().join(", ")
        )
    );
    let captured = crate::json::JsonSource::capture(&path).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    // Truncation, not growth: an append-only log may grow under a captured source.
    std::fs::write(&path, b"").unwrap();
    assert!(retrieve_captured(&captured, &file, &path, "id-0").is_err());
}

#[test]
fn retrieve_giant_unselected_fields_return_only_requested_content() {
    let world = crate::test_world("retrieve-giant-unselected");
    let path = world.base.join("session.flat.jsonl");
    let mut out = std::fs::File::create(&path).unwrap();
    let chunk = "ignored-🌙\\n\\\"\\\\".repeat(4096);
    out.write_all(br#"{"id":"other-"#).unwrap();
    for _ in 0..32 {
        out.write_all(chunk.as_bytes()).unwrap();
    }
    out.write_all(br#"","slot":"content","name":""#).unwrap();
    for _ in 0..32 {
        out.write_all(chunk.as_bytes()).unwrap();
    }
    out.write_all(br#"","input":{"body":""#).unwrap();
    for _ in 0..32 {
        out.write_all(chunk.as_bytes()).unwrap();
    }
    out.write_all(br#""},"content":{"body":""#).unwrap();
    for _ in 0..32 {
        out.write_all(chunk.as_bytes()).unwrap();
    }
    out.write_all(br#""},"size":1,"line_count":1,"kind":"text"}"#).unwrap();
    out.write_all(b"\n").unwrap();
    let chosen = serde_json::json!({"actual":["雪🌙",true,null]});
    let entry: SidecarEntry = SidecarEntry {
        id: "wanted".into(),
        slot: "content".into(),
        name: "tool".into(),
        input: Value::Null,
        content: chosen.clone(),
        size: 1,
        line_count: 1,
        kind: "text".into(),
    };
    serde_json::to_writer(&mut out, &entry).unwrap();
    drop(out);
    assert_eq!(retrieve(&path, "wanted").unwrap(), chosen);
    assert!(retrieve(&path, "absent").unwrap_err().to_string().ends_with(", wanted"));
}

#[test]
fn source_turn_projection_matches_frozen_composition() -> Result<()> {
    let world = crate::test_world("prune-turn-projection");
    for (case, mut original) in [
        serde_json::json!({"type":"assistant","uuid":"a","sessionId":"old","parentUuid":"p","message":{"content":[
            {"type":"thinking","thinking":"reason\r\nsecond λ\n","signature":"signature","unknown":{"keep":true}},
            {"type":"text","text":"ALPHA beta ΟΣ ΟΣΑ","extra":"retained in sidecar"},
            {"type":"thinking","signature":"signature-only"},
            {"type":"text","text":42},
            {"type":"text","text":"benign"}
        ]}}),
        serde_json::json!({"type":"user","uuid":"u","sessionId":"old","message":{"content":[
            {"type":"text","text":"ALPHA beta"},
            {"type":"tool_result","tool_use_id":"call","content":"payload"},
            {"type":"text","text":"benign"}
        ]},"toolUseResult":{"mirror":"payload"}}),
        serde_json::json!({"type":"user","uuid":"u","session_id":"old","message":{"content":"ALPHA beta\r\n"}}),
    ]
    .into_iter()
    .enumerate()
    {
        original["ignored"] = Value::String("not selected".repeat(100000));
        let raw = serde_json::to_string_pretty(&original)?.replace(
            "\"sessionId\": \"old\"",
            "\"sessionId\":\"first\", \"sessionId\":\"old\"",
        );
        let path = world.base.join(format!("{case}.json"));
        std::fs::write(&path, &raw)?;
        for drop in [false, true] {
            for thinking in [false, true] {
                for min_hits in [0, 2] {
                    let opts = PruneOptions {
                        min_size: 1,
                        drop,
                        thinking,
                        revive: true,
                        declassify: true,
                        declassify_min_hits: min_hits,
                        declassify_tokens: vec!["alpha".into(), "beta".into(), "ος".into()],
                        ..Default::default()
                    };
                    let user = original["type"] == "user";
                    let source = crate::json::JsonSource::capture(&path)?;
                    let plan = source_turn_prune(
                        &source,
                        Range {
                            offset: 0,
                            len: raw.len() as u64,
                        },
                        &opts,
                        "new",
                        0,
                        &HashMap::new(),
                        user,
                        thinking && !user,
                    )?;
                    assert_frozen_plan(&format!("turn-{case}-{drop}-{thinking}-{min_hits}"), plan.as_ref());
                }
            }
        }
    }
    Ok(())
}

#[test]
fn large_noop_declassification_preserves_raw_duplicate_ids_and_spacing() -> Result<()> {
    let world = crate::test_world("prune-noop-turn-source");
    let path = world.base.join("source.jsonl");
    let raw = format!("  {{\"type\":\"assistant\",\"sessionId\":\"first\",\"sessionId\":\"old\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}  ",
        serde_json::to_string(&"BENIGN prose\n".repeat(131072))?);
    std::fs::write(&path, format!("{raw}\r\n"))?;
    let result = prune_session(
        crate::Harness::Claude,
        &path,
        &PruneOptions {
            declassify: true,
            declassify_tokens: vec!["alpha".into(), "beta".into()],
            new_id: Some("new".into()),
            ..Default::default()
        },
    )?;
    assert_eq!(result.pruned_count, 0);
    assert!(result.sidecar_path.is_none());
    let actual = std::fs::read(result.new_path)?;
    // Positioned JSONL framing keeps leading bytes and trims trailing record whitespace.
    let expected = format!("{}\n", raw.trim_end());
    assert!(actual == expected.as_bytes(), "no-op record spacing or CRLF changed");
    Ok(())
}

#[test]
fn source_usage_projection_matches_owned_order_and_iterations() -> Result<()> {
    let world = crate::test_world("prune-usage-projection");
    for (index, original_ctx) in [
        None,
        Some(Value::Null),
        Some(Value::String("keep".into())),
        Some(Value::from(7)),
    ]
    .into_iter()
    .enumerate()
    {
        let mut original = serde_json::json!({"z":"first","message":{"content":[{"type":"text","text":"keep body λ"}],"usage":{
            "output_tokens":8,"cache_read_input_tokens":4,"input_tokens":900,"cache_creation_input_tokens":3,
            "cache_creation":{"unknown":"keep","ephemeral_5m_input_tokens":2},
            "iterations":[null,{"type":"advisor_message","input_tokens":80},{"type":"compaction","input_tokens":90},
                {"type":"fallback_message","output_tokens":4,"input_tokens":700,"cache_read_input_tokens":5,"cache_creation_input_tokens":6},
                {"other":"still pinned"}],"unknown":{"a":[1,2,3]}
        }},"a":"last"});
        if let Some(value) = original_ctx {
            original["_cv_orig_ctx"] = value;
        }
        let raw =
            serde_json::to_string(&original)?.replace("\"z\":\"first\"", "\"z\":\"ignored duplicate\",\"z\":\"first\"");
        let path = world.base.join(format!("{index}.json"));
        std::fs::write(&path, &raw)?;
        let source = crate::Json::source(&path, 0, raw.len() as u64)?;
        let mut expected: Value = serde_json::from_str(&raw)?;
        if expected.get("_cv_orig_ctx").is_none() {
            expected["_cv_orig_ctx"] = Value::from(900);
        }
        pin_usage(
            expected.pointer_mut("/message/usage").unwrap().as_object_mut().unwrap(),
            31,
        );
        let mut actual = Vec::new();
        source.with_view(|view| write_source_usage(view, 900, 31, &mut actual))?;
        assert_eq!(actual, serde_json::to_vec(&expected)?);
    }
    Ok(())
}

#[test]
fn staged_usage_projection_preserves_mixed_raw_framing_and_source_guard() -> Result<()> {
    let world = crate::test_world("prune-usage-staged");
    let path = world.base.join("source.jsonl");
    let mut row = serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"small"}],"usage":{"input_tokens":900,"output_tokens":7}}});
    let raw = format!("invalid {{ \r\n  {row}  \r\nlast raw row\n");
    std::fs::write(&path, &raw)?;
    let source = crate::json::JsonSource::capture(&path)?;
    let mut stage = ByteStage::new();
    stage.write_all(raw.as_bytes())?;
    let facts = [None, fixture_decisions(std::slice::from_ref(&row)).remove(0), None];
    let (output, rewritten, honest, stale) =
        revive_staged(&source, stage.finish()?, &facts, &[false; 3], None, true, None)?;
    assert_eq!(rewritten, 1);
    assert_eq!(stale, Some(900));
    row["_cv_orig_ctx"] = Value::from(900);
    pin_usage(
        row.pointer_mut("/message/usage").unwrap().as_object_mut().unwrap(),
        honest.unwrap(),
    );
    let expected = format!("invalid {{ \r\n{row}\nlast raw row\n");
    let mut actual = Vec::new();
    output.reader()?.read_to_end(&mut actual)?;
    assert_eq!(actual, expected.as_bytes());
    let mut stage = ByteStage::new();
    stage.write_all(raw.as_bytes())?;
    std::fs::write(&path, b"changed source")?;
    assert!(revive_staged(&source, stage.finish()?, &facts, &[false; 3], None, true, None).is_err());
    Ok(())
}

#[test]
fn staged_usage_trim_preserves_whitespace_only_buffers_and_eof_failure() -> Result<()> {
    let world = crate::test_world("prune-usage-trim-buffers");
    let path = world.base.join("source.jsonl");
    let row = serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"small"}],"usage":{"input_tokens":900,"output_tokens":7}}});
    let raw = format!("{}{}{}\r\n", " ".repeat(2 * 65536), row, "\t".repeat(2 * 65536));
    std::fs::write(&path, &raw)?;
    let source = crate::json::JsonSource::capture(&path)?;
    let facts = [fixture_decisions(std::slice::from_ref(&row)).remove(0)];
    let mut stage = ByteStage::new();
    stage.write_all(raw.as_bytes())?;
    let (output, rewritten, honest, _) = revive_staged(&source, stage.finish()?, &facts, &[false], None, true, None)?;
    assert_eq!(rewritten, 1);
    let mut expected = row;
    expected["_cv_orig_ctx"] = Value::from(900);
    pin_usage(
        expected.pointer_mut("/message/usage").unwrap().as_object_mut().unwrap(),
        honest.unwrap(),
    );
    let mut actual = Vec::new();
    output.reader()?.read_to_end(&mut actual)?;
    assert_eq!(actual, format!("{expected}\n").as_bytes());
    // The producer promises newline framing; truncated private rows still fail at EOF.
    let mut stage = ByteStage::new();
    stage.write_all(raw.trim_end_matches('\n').as_bytes())?;
    assert!(revive_staged(&source, stage.finish()?, &facts, &[false], None, true, None).is_err());
    Ok(())
}

#[test]
fn linked_tool_input_projection_matches_frozen_owned_lookup() -> Result<()> {
    let world = crate::test_world("prune-linked-input");
    let path = world.base.join("source.jsonl");
    let raw = format!(
        "{{\"type\":\"assistant\",\"ignored\":{},\"message\":{{\"content\":[\
        {{\"type\":\"thinking\",\"thinking\":\"unrelated\"}},\
        {{\"type\":\"tool_use\",\"id\":\"duplicate\",\"name\":\"first\",\"input\":{{\"a\":1,\"a\":2}}}},\
        {{\"type\":\"tool_use\",\"id\":\"missing\",\"name\":\"NoInput\"}},\
        {{\"type\":\"tool_use\",\"id\":\"duplicate\",\"name\":\"last\",\"input\":[{{\"keep\":\"λ\"}}]}}]}}}}",
        serde_json::to_string(&"x".repeat(2 * 1024 * 1024))?
    );
    std::fs::write(&path, &raw)?;
    let source = crate::json::JsonSource::capture(&path)?;
    let range = Range {
        offset: 0,
        len: raw.len() as u64,
    };
    let mut tools = HashMap::new();
    source.read(ReadInput::Range(0, range.len), Validation::Value, |view| {
        decide(
            view,
            range,
            &PruneOptions::default(),
            &mut tools,
            &mut PlanCheck::default(),
        )
        .map(|_| ())
    })?;
    assert_eq!(
        source_tool_metadata(&source, Some("duplicate"), &tools)?,
        ("last".into(), serde_json::json!([{"keep":"λ"}]))
    );
    assert_eq!(
        source_tool_metadata(&source, Some("missing"), &tools)?,
        ("NoInput".into(), Value::Null)
    );
    std::fs::write(&path, b"unavailable original metadata")?;
    assert!(source_tool_metadata(&source, Some("duplicate"), &tools).is_err());
    Ok(())
}

#[test]
fn malformed_large_rows_refuse_before_staging_and_invalid_utf8_is_named_first() -> Result<()> {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let source = dir.join("source.jsonl");
    let prefix = " malformed sessionId=old ";
    let malformed = format!(
        "{prefix}{}[PRUNED id=ignored] {}🙂  \r",
        "x".repeat(65535 - prefix.len() - 5),
        "x".repeat(2 << 20)
    );
    let valid = r#" {"sessionId":"old","type":"user","uuid":"u","message":{"content":"kept"}} "#;
    // Frozen legacy framing for readable input: global trim_end of JSON whitespace, id surgery
    // on the top-level ids, one LF per retained record.
    let readable = format!("{valid}\r\n \t \r\n");
    std::fs::write(&source, &readable)?;
    let expected = crate::restamp_line(readable.trim_end_matches([' ', '\t', '\r', '\n']), "new").unwrap() + "\n";
    let result = prune_session(
        crate::Harness::Claude,
        &source,
        &PruneOptions {
            new_id: Some("new".into()),
            ..Default::default()
        },
    )?;
    assert_eq!(std::fs::read_to_string(&result.new_path)?, expected);
    assert_eq!(result.source_id, "old");
    let previous = std::fs::read(dir.join("new.jsonl"))?;
    // A row that is not one JSON value, past the in-memory limit or a lone U+2003 (a record, not
    // JSON whitespace), is refused by line before anything is staged, with or without revive.
    for (text, line) in [
        (format!("{malformed}\n{valid}\r\n\u{2003} \r\n"), 1),
        (format!("{valid}\r\n\u{2003} \r\n"), 2),
    ] {
        std::fs::write(&source, &text)?;
        for revive in [false, true] {
            let error = prune_session(
                crate::Harness::Claude,
                &source,
                &PruneOptions {
                    new_id: Some("new".into()),
                    revive,
                    force: true,
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(error.is::<crate::json::InvalidJson>(), "{error:#}");
            assert!(
                error
                    .to_string()
                    .contains(&format!("line {line} is not a complete JSON record")),
                "{error:#}"
            );
            assert_eq!(std::fs::read(dir.join("new.jsonl"))?, previous, "nothing replaced");
            assert_eq!(std::fs::read_to_string(&source)?, text);
        }
    }
    // Invalid UTF-8 inside the malformed row is named as such, before the malformed refusal.
    let mut invalid = format!("{malformed}\n{valid}\r\n").into_bytes();
    invalid[1 << 20] = 0xff;
    std::fs::write(&source, invalid)?;
    let error = prune_session(
        crate::Harness::Claude,
        &source,
        &PruneOptions {
            new_id: Some("new".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(error.to_string(), "stream did not contain valid UTF-8");
    assert_eq!(std::fs::read(dir.join("new.jsonl"))?, previous);

    Ok(())
}

#[test]
fn rejected_large_json_keeps_strict_owned_value_acceptance() -> Result<()> {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let path = dir.join("source.jsonl");
    let padding = " ".repeat(1024 * 1024);
    for raw in [
        "{bad}".to_owned(),
        "1e400".to_owned(),
        r#"{"sessionId":"old","ignored":1e400}"#.to_owned(),
        r#""\ud800""#.to_owned(),
        r#"{"sessionId":"old","ignored":"\ud800"}"#.to_owned(),
        format!("{}0{}", "[".repeat(130), "]".repeat(130)),
        r#"{"sessionId":"old","type":"user","message":{"content":"kept"}}"#.to_owned(),
    ] {
        let row = format!("{padding}{raw}\n");
        std::fs::write(&path, &row)?;
        let accepted = serde_json::from_str::<Value>(&row).is_ok();
        let result = prune_session(
            crate::Harness::Claude,
            &path,
            &PruneOptions {
                new_id: Some("new".into()),
                force: true,
                ..Default::default()
            },
        );
        // Exactly the rows serde's Value rejects are refused, by line, instead of copied.
        match result {
            Ok(result) => {
                assert!(accepted, "{raw}");
                let expected = crate::restamp_line(row.trim_end(), "new").unwrap() + "\n";
                assert_eq!(std::fs::read_to_string(result.new_path)?, expected);
            }
            Err(error) => {
                assert!(!accepted && error.is::<crate::json::InvalidJson>(), "{raw}: {error:#}");
                assert!(
                    error.to_string().contains("line 1 is not a complete JSON record"),
                    "{error:#}"
                );
                assert!(!dir.join("new.jsonl").exists(), "{raw}: nothing published");
            }
        }
    }

    Ok(())
}

/// Native children carry their parent's sessionId and sidechain flags on every own turn.
fn native_child_fixture(base: &Path, big: &str) -> Result<(PathBuf, PathBuf, Vec<u8>)> {
    let project = base.join("projects/fixture");
    std::fs::create_dir_all(&project)?;
    let original = fixture(&project, big);
    let before = std::fs::read(&original)?;
    let parent = original.file_stem().unwrap().to_str().unwrap();
    let subdir = project.join(parent).join("subagents");
    std::fs::create_dir_all(&subdir)?;
    let child = subdir.join("agent-native-child.jsonl");
    let mut rows = String::new();
    for row in std::str::from_utf8(&before)?.lines() {
        let mut value: Value = serde_json::from_str(row)?;
        value["isSidechain"] = Value::Bool(true);
        value["agentId"] = Value::String("native-child".into());
        if value["uuid"] == "u0" {
            value["parentUuid"] = Value::String("parent-turn".into());
        }
        rows.push_str(&value.to_string());
        rows.push('\n');
    }
    std::fs::write(&child, rows)?;
    Ok((original, child, before))
}

#[test]
fn subagent_prune_counts_own_turns_and_emits_standalone_without_touching_parent() -> Result<()> {
    let world = crate::test_world("prune-native-child");
    for size in [4096, 1100000] {
        let base = world.base.join(size.to_string());
        let (parent, child, parent_before) = native_child_fixture(&base, &"x".repeat(size))?;
        let child_before = std::fs::read(&child)?;
        let opts = PruneOptions {
            keep_last: 2,
            min_size: 100,
            new_id: Some("child-pruned".into()),
            ..Default::default()
        };
        // Child resources are distinct from its parent's other children/workflows.
        let parent_resources = parent.with_extension("");
        std::fs::write(
            parent_resources.join("unrelated-parent-resource"),
            "leave parent resources alone",
        )?;
        let child_resources = child.with_extension("");
        std::fs::create_dir_all(&child_resources)?;
        std::fs::write(child_resources.join("child-resource"), "own")?;
        let result = prune_session(crate::Harness::Claude, &child, &opts)?;
        assert_eq!(result.source_id, "agent-native-child");
        assert_eq!(
            result.parent_session_id.as_deref(),
            parent.file_stem().and_then(|x| x.to_str())
        );
        assert_eq!(
            result.pruned_count, 2,
            "old child result and mirror are snipped; recent result survives"
        );
        assert_eq!(result.new_path.parent(), parent.parent());
        let output = std::fs::read_to_string(&result.new_path)?;
        let rows: Vec<Value> = output
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        for row in &rows {
            assert_eq!(row["sessionId"], "child-pruned");
            assert_eq!(row["isSidechain"], false);
            assert!(row.get("agentId").is_none());
        }
        assert!(rows[0]["parentUuid"].is_null());
        assert!(rows[2]["message"]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains(MARKER_PREFIX));
        assert_eq!(rows[4]["message"]["content"][0]["content"], "x".repeat(size));
        assert_eq!(std::fs::read(&parent)?, parent_before);
        assert_eq!(std::fs::read(&child)?, child_before);
        let copied = prune_session(
            crate::Harness::Claude,
            &child,
            &PruneOptions {
                copy_resources: true,
                new_id: Some("child-resources-pruned".into()),
                ..opts.clone()
            },
        )?;
        let resources = copied.copied_resources.unwrap();
        assert!(resources.join("child-resource").is_file());
        assert!(!resources.join("unrelated-parent-resource").exists());
        let mut collision = opts.clone();
        collision.new_id = result.parent_session_id;
        assert!(prune_session(crate::Harness::Claude, &child, &collision).is_err());
        assert_eq!(std::fs::read(&parent)?, parent_before);
    }
    Ok(())
}

#[test]
fn subagent_prune_keep_window_and_usage_match_standalone_turn_semantics() -> Result<()> {
    let world = crate::test_world("prune-native-child-window");
    let (parent, child, _) = native_child_fixture(&world.base, &"x".repeat(4096))?;
    for (label, keep_range, window) in [("keep", Some((3, None)), None), ("window", None, Some(20))] {
        let opts = PruneOptions {
            keep_range,
            window,
            new_id: Some(format!("child-{label}")),
            ..Default::default()
        };
        let expected = prune_session(
            crate::Harness::Claude,
            &parent,
            &PruneOptions {
                new_id: Some(format!("parent-{label}")),
                ..opts.clone()
            },
        )?;
        let actual = prune_session(crate::Harness::Claude, &child, &opts)?;
        assert!(actual.dropped_turns > 0);
        assert_eq!(actual.dropped_turns, expected.dropped_turns);
        let normalized = |path: &Path| -> Result<Vec<Value>> {
            std::fs::read_to_string(path)?
                .lines()
                .map(|line| {
                    let mut value: Value = serde_json::from_str(line)?;
                    if let Some(object) = value.as_object_mut() {
                        object.remove("sessionId");
                        object.remove("session_id");
                        object.remove("isSidechain");
                    }
                    Ok(value)
                })
                .collect()
        };
        assert_eq!(normalized(&actual.new_path)?, normalized(&expected.new_path)?);
    }
    // Provider-recorded usage belongs to this selected child, rather than an embedded sidechain.
    let mut rows: Vec<Value> = std::fs::read_to_string(&child)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    for (index, row) in rows.iter_mut().enumerate() {
        if row["type"] == "assistant" {
            row["message"]["usage"] = serde_json::json!({"input_tokens": (index + 1) * 100});
        }
    }
    std::fs::write(&child, rows.iter().map(|v| format!("{v}\n")).collect::<String>())?;
    let actual = prune_session(
        crate::Harness::Claude,
        &child,
        &PruneOptions {
            window: Some(250),
            revive: true,
            new_id: Some("child-usage".into()),
            ..Default::default()
        },
    )?;
    assert!(actual.window_real_tokens.is_some());
    assert!(actual.dropped_turns > 0);
    Ok(())
}

#[test]
fn parent_prune_keeps_embedded_sidechains_out_of_own_turns() -> Result<()> {
    let world = crate::test_world("prune-embedded-child");
    let (parent, child, before) = native_child_fixture(&world.base, &"x".repeat(4096))?;
    let embedded = std::fs::read(&child)?;
    let mut mixed = before;
    mixed.extend_from_slice(&embedded);
    std::fs::write(&parent, &mixed)?;
    let result = prune_session(
        crate::Harness::Claude,
        &parent,
        &PruneOptions {
            keep_last: 2,
            min_size: 100,
            new_id: Some("parent-pruned".into()),
            ..Default::default()
        },
    )?;
    assert!(result.parent_session_id.is_none());
    let rows: Vec<Value> = std::fs::read_to_string(&result.new_path)?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    assert!(rows.last().unwrap()["isSidechain"].as_bool().unwrap());
    assert_eq!(rows.last().unwrap()["agentId"], "native-child");
    assert_eq!(std::fs::read(parent)?, mixed);
    Ok(())
}

#[test]
fn prune_rejects_output_ids_that_escape_or_alias_resource_directories() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let source = fixture(&dir, "original payload");
    let original = std::fs::read(&source).unwrap();
    for id in [
        "",
        ".",
        "..",
        "../victim",
        "/tmp/victim",
        "nested/child",
        "nested\\child",
        "bad\0id",
    ] {
        let error = prune_session(
            crate::Harness::Claude,
            &source,
            &PruneOptions {
                new_id: Some(id.into()),
                copy_resources: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("filename component"), "{error}");
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
}

/// `--copy-resources` never merges into an existing resource directory: it is refused, and with
/// `force` the new directory replaces it whole (stale entries gone, a symlink replaced, not followed).
#[test]
fn copied_resources_are_refused_or_replaced_whole_never_merged() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let source = fixture(&dir, &"payload".repeat(1024));
    let resources = dir.join(source.file_stem().unwrap()).join("subagents");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(resources.join("agent-a.jsonl"), b"agent transcript").unwrap();
    let new_id = "22222222-2222-4222-8222-222222222222";
    let destination = dir.join(new_id);
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("stale.txt"), b"another session's resource").unwrap();
    let mut options = PruneOptions {
        new_id: Some(new_id.into()),
        keep_last: 0,
        copy_resources: true,
        ..Default::default()
    };
    let error = prune_session(crate::Harness::Claude, &source, &options).unwrap_err();
    assert!(error.downcast_ref::<crate::publish::Exists>().is_some(), "{error:#}");
    assert!(
        !dir.join(format!("{new_id}.jsonl")).exists(),
        "a refused prune writes nothing"
    );
    assert_eq!(
        std::fs::read(destination.join("stale.txt")).unwrap(),
        b"another session's resource"
    );
    options.force = true;
    prune_session(crate::Harness::Claude, &source, &options).unwrap();
    assert!(!destination.join("stale.txt").exists(), "replaced, not merged");
    assert_eq!(
        std::fs::read(destination.join("subagents/agent-a.jsonl")).unwrap(),
        b"agent transcript"
    );
    assert_eq!(
        std::fs::read(resources.join("agent-a.jsonl")).unwrap(),
        b"agent transcript"
    );
    #[cfg(unix)]
    {
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("keep.txt"), b"kept").unwrap();
        std::fs::remove_dir_all(&destination).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &destination).unwrap();
        prune_session(crate::Harness::Claude, &source, &options).unwrap();
        assert!(!std::fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read(elsewhere.join("keep.txt")).unwrap(), b"kept");
    }
    assert!(!std::fs::read_dir(&dir)
        .unwrap()
        .any(|e| e.unwrap().file_name().to_string_lossy().starts_with(".cv-")));
}

#[test]
fn resource_destination_that_contains_the_source_is_refused() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let source = dir.join("outer/source-resources");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("agent.jsonl"), b"original resource").unwrap();
    assert!(stage_resources(&source, &dir.join("outer")).is_err());
    assert_eq!(std::fs::read(source.join("agent.jsonl")).unwrap(), b"original resource");
}

#[test]
fn byte_window_accounts_for_trailing_reminders_and_prompt_snap_overshoot() {
    let options = PruneOptions {
        window: Some(100),
        ..Default::default()
    };
    let message = |role: &str, text: &str| serde_json::json!({"type":role,"message":{"content":text}});
    let rows = fixture_decisions(&[
        message("user", "first"),
        message("assistant", "answer"),
        message("user", "latest"),
        message("assistant", "reply"),
        serde_json::json!({"type":"attachment","rendered":[{"content":"x".repeat(1000)}]}),
    ]);
    assert_eq!(select_kept_turns(&rows, &options, 4), (2, 4, None, true));
    let rows = fixture_decisions(&[
        message("user", "first"),
        message("assistant", "answer"),
        message("user", &"x".repeat(1000)),
        message("assistant", "reply"),
    ]);
    assert_eq!(select_kept_turns(&rows, &options, 4), (2, 4, None, true));
    let options = PruneOptions {
        window: Some(1000),
        ..options
    };
    assert_eq!(select_kept_turns(&rows, &options, 4), (0, 4, None, false));
}

// Frozen from67ad8bf9 before deleting the owned transform twins. Keep output/sidecar bytes and
// estimates covered without maintaining a second implementation of the transform.
fn assert_frozen_plan(key: &str, plan: Option<&SourcePrune>) {
    let actual = if let Some(p) = plan {
        let mut output = Vec::new();
        p.record
            .with_view(|v| {
                let rewrite = crate::derive::Rewrite {
                    session_id: "new",
                    standalone: false,
                    identity: crate::harness::claude::IDENTITY,
                };
                crate::derive::write_modified(v, &p.replacements, &rewrite, &mut output)
            })
            .unwrap();
        let mut sidecar = Vec::new();
        for payload in &p.payloads {
            write_source_payload(payload, &mut sidecar).unwrap();
        }
        // Bounded bytes read as text in a failure diff; output and payloads past the bound keep a
        // hash so the committed fixture stays small.
        let frozen = |bytes: &[u8]| {
            const INLINE_BYTES: usize = 4096;
            if bytes.len() <= INLINE_BYTES {
                serde_json::Value::String(String::from_utf8(bytes.to_vec()).expect("frozen output is UTF-8"))
            } else {
                let mut h = crate::digest::Sha256::new();
                h.update(bytes);
                serde_json::Value::String(h.finish_hex())
            }
        };
        serde_json::json!({"key": key, "output": frozen(&output), "sidecar": frozen(&sidecar), "count":p.payloads.len(), "images":p.images,"saved":p.saved,"estimate":[p.estimate.tokens,p.estimate.signature,p.estimate.images]})
    } else {
        serde_json::json!({"key":key,"none":true})
    };
    let expected = include_str!("../../tests/fixtures/prune/projections.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|row| row["key"] == key)
        .unwrap();
    assert_eq!(actual, expected, "{key}");
}

#[test]
fn standalone_prune_preserves_unmodified_metadata_bytes_at_every_size() {
    for padding in [0, 1024 * 1024] {
        let world = crate::test_world("prune-standalone-raw-metadata");
        let path = world.base.join("agent-child.jsonl");
        let metadata = format!(
            "{{ \"type\" : \"system\", \"sessionId\" : \"old\", \"value\" : 1.00, \"padding\":\"{}\" }}",
            "x".repeat(padding)
        );
        let user = r#"{"type":"user","sessionId":"old","isSidechain":true,"agentId":"child","uuid":"u","parentUuid":"parent","message":{"content":"prompt"}}"#;
        std::fs::write(&path, format!("{metadata}\n{user}\n")).unwrap();
        let result = prune_session(
            crate::Harness::Claude,
            &path,
            &PruneOptions {
                new_id: Some("derived".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let output = std::fs::read_to_string(result.new_path).unwrap();
        assert!(output.starts_with(&format!("{}\n", crate::restamp_line(&metadata, "derived").unwrap())));
        let second: Value = serde_json::from_str(output.lines().nth(1).unwrap()).unwrap();
        assert_eq!(second["isSidechain"], false);
        assert!(second.get("agentId").is_none());
        assert!(second["parentUuid"].is_null());
    }
}

/// A malformed line inside the kept window is a record Claude Code cannot read back; a prune
/// that copied it verbatim would publish an unresumable session. Refuse before staging.
#[test]
fn prune_refuses_a_malformed_line_inside_the_kept_window() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let sid = "44444444-4444-4444-8444-444444444444";
    let line = |uuid: &str, parent: Option<&str>, role: &str, content: &str| {
        serde_json::json!({"type":role,"sessionId":sid,"uuid":uuid,"parentUuid":parent,
            "timestamp":"2026-06-18T00:00:00Z","message":{"role":role,"content":content}})
        .to_string()
    };
    let input = [
        line("u0", None, "user", "start"),
        line("a0", Some("u0"), "assistant", "ok"),
        "{\"type\":\"user\",\"uuid\":\"u1\",\"parentUuid\":\"a0\",\"message\":{\"role\":\"user\",\"content\":\"cut sho"
            .to_string(),
        line("a1", Some("u1"), "assistant", "done"),
    ]
    .join("\n")
        + "\n";
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(&path, &input).unwrap();
    // The window that would drop the line does not excuse it either: the source is unreadable.
    for keep_range in [None, Some((1, None))] {
        let opts = PruneOptions {
            new_id: Some("55555555-5555-4555-8555-555555555555".into()),
            keep_range,
            ..Default::default()
        };
        let error = prune_session(crate::Harness::Claude, &path, &opts).unwrap_err();
        assert!(error.is::<crate::json::InvalidJson>(), "{error:#}");
        let message = error.to_string();
        assert!(message.contains("line 3 is not a complete JSON record"), "{message}");
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(
            !dir.join("55555555-5555-4555-8555-555555555555.jsonl").exists(),
            "nothing is published when the source is unreadable"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        input,
        "the source is untouched"
    );
}

/// Append a record to every file `tag` staged in `dir`: a turn whose parent is in neither file, so
/// the resume walk from it loses the whole session.
pub(crate) fn break_staged(dir: &Path, tag: &str) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(&format!(".cv-{tag}-")))
        {
            let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
            std::io::Write::write_all(
                &mut file,
                br#"{"type":"user","uuid":"stray","parentUuid":"nowhere","message":{"role":"user","content":"x"}}
"#,
            )?;
        }
    }
    Ok(())
}

/// The staged session is verified resumable against its source before it is published: a break
/// the source does not have refuses the prune, and neither the session nor its sidecar is left.
#[test]
fn a_staged_prune_that_breaks_resume_is_refused_and_leaves_nothing() {
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let src = fixture(&dir, &"Y".repeat(5000));
    let before = std::fs::read(&src).unwrap();
    let opts = PruneOptions {
        min_size: 2048,
        keep_last: 0,
        new_id: Some("aaaa".into()),
        ..Default::default()
    };
    let err = prune_session_after_stage(crate::Harness::Claude, &src, &opts, || break_staged(&dir, "prune"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("aaaa.jsonl:"), "{err}");
    assert!(err.contains("parentUuid nowhere names no record"), "{err}");
    let mut left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    assert_eq!(left, [src.file_name().unwrap().to_string_lossy()]);
    assert_eq!(std::fs::read(&src).unwrap(), before);
    // Untampered, the same prune publishes.
    prune_session(crate::Harness::Claude, &src, &opts).unwrap();
}

/// A live transcript appended to while it is pruned publishes the prefix read and says it grew;
/// one whose read prefix is rewritten in place refuses.
#[test]
fn a_live_source_grown_during_prune_publishes_its_prefix_and_a_rewrite_refuses() {
    let opts = |id: &str| PruneOptions {
        min_size: 2048,
        keep_last: 0,
        new_id: Some(id.into()),
        ..Default::default()
    };
    let world = crate::test_world("prune");
    let src = fixture(&world.proj, &"Y".repeat(5000));
    let len = std::fs::metadata(&src).unwrap().len();
    let expected = prune_session(crate::Harness::Claude, &src, &opts("aaaa")).unwrap();
    let res = prune_session_after_stage(crate::Harness::Claude, &src, &opts("bbbb"), || {
        let mut file = std::fs::OpenOptions::new().append(true).open(&src)?;
        std::io::Write::write_all(
            &mut file,
            b"{\"type\":\"user\",\"uuid\":\"later\",\"message\":{\"role\":\"user\",\"content\":\"later\"}}\n",
        )?;
        Ok(())
    })
    .unwrap();
    assert_eq!(res.original_size, len);
    assert_eq!(res.new_size, expected.new_size);
    let now = std::fs::metadata(&src).unwrap().len();
    let note = format!("read as of byte offset {len}; the source has grown since ({now} bytes now)");
    assert_eq!(res.warnings, [note]);
    let err = prune_session_after_stage(crate::Harness::Claude, &src, &opts("cccc"), || {
        let mut bytes = std::fs::read(&src)?;
        bytes[1] = b' ';
        let mut file = std::fs::OpenOptions::new().write(true).open(&src)?;
        std::io::Write::write_all(&mut file, &bytes)?;
        Ok(())
    })
    .unwrap_err();
    assert!(err.to_string().contains("JSON source changed"), "{err}");
}

/// An empty or blank-only source holds nothing to prune: refused, nothing published.
#[test]
fn an_empty_source_is_refused_without_publication() {
    for body in ["", "\n \n\t\n"] {
        let world = crate::test_world("prune");
        let dir = world.proj.clone();
        let src = dir.join("11111111-1111-4111-8111-111111111111.jsonl");
        std::fs::write(&src, body).unwrap();
        let opts = PruneOptions {
            new_id: Some("aaaa".into()),
            ..Default::default()
        };
        let err = prune_session(crate::Harness::Claude, &src, &opts).unwrap_err();
        assert!(err.to_string().contains("holds no records; nothing to prune"), "{err}");
        assert!(!dir.join("aaaa.jsonl").exists());
    }
    // An empty line between records is copied like any blank one.
    let world = crate::test_world("prune");
    let dir = world.proj.clone();
    let src = fixture(&dir, "small");
    let body = std::fs::read_to_string(&src).unwrap().replacen('\n', "\n\n", 1);
    std::fs::write(&src, &body).unwrap();
    let opts = PruneOptions {
        new_id: Some("aaaa".into()),
        ..Default::default()
    };
    let r = prune_session(crate::Harness::Claude, &src, &opts).unwrap();
    assert_eq!(std::fs::read_to_string(r.new_path).unwrap().lines().nth(1), Some(""));
}

/// A minted session id is time-ordered (v7), like every other id cv mints.
#[test]
fn a_minted_session_id_is_v7() {
    let id = uuid::Uuid::parse_str(&new_uuid()).unwrap();
    assert_eq!(id.get_version_num(), 7);
}
