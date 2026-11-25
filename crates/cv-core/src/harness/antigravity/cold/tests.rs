use super::*;
use crate::harness::claude::skills::SkillsPolicy;
use serde_json::json;

const OLD: &str = "11111111-1111-4111-8111-111111111111";
const TID: &str = "22222222-2222-4222-8222-222222222222";
const CHILDREN: [&str; 4] = [
    "31111111-1111-4111-8111-111111111111",
    "41111111-1111-4111-8111-111111111111",
    "51111111-1111-4111-8111-111111111111",
    "61111111-1111-4111-8111-111111111111",
];
fn field(number: u32, v: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    proto::bytes(&mut out, number, v);
    out
}
fn int(number: u32, v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    proto::scalar(&mut out, number, v);
    out
}
fn metadata(i: u64) -> Vec<u8> {
    let mut r = field(1, TID.as_bytes());
    if i != 0 {
        r.extend(int(2, i));
        r.extend(int(3, if i < 140 { 1 } else { 2 }));
    }
    r.extend(field(4, OLD.as_bytes()));
    field(20, &r)
}
fn prompt() -> String {
    let mut text = format!("The historical ID is {OLD}.\n");
    text.push_str(&"x".repeat(321_080 - text.len()));
    text
}
fn fixture(root: &Path) -> Result<(SessionRef, SessionRef)> {
    let db = root.join("conversations").join(format!("{OLD}.db"));
    fs::create_dir_all(db.parent().unwrap())?;
    let conn = Connection::open(&db)?;
    conn.execute_batch(SCHEMA)?;
    conn.execute("INSERT INTO trajectory_meta VALUES(?,?,1,1)", [TID, OLD])?;
    conn.execute(
        "INSERT INTO trajectory_metadata_blob VALUES('main',?)",
        [field(6, OLD.as_bytes())],
    )?;
    let brain = root.join("brain").join(OLD);
    let log = brain.join(".system_generated/logs/transcript_full.jsonl");
    fs::create_dir_all(log.parent().unwrap())?;
    fs::create_dir_all(brain.join("scratch"))?;
    fs::write(brain.join("scratch/notes.txt"), "source artifact")?;
    let descriptors = brain.join(".system_generated/subagents");
    fs::create_dir_all(&descriptors)?;
    for id in CHILDREN {
        fs::write(
            descriptors.join(format!("{id}.json")),
            json!({"conversationId":id,"spawnStepIndex":1,"state":"SUBAGENT_STATE_IDLE"}).to_string(),
        )?;
    }
    let child_log = root
        .join("brain")
        .join(CHILDREN[3])
        .join(".system_generated/logs/transcript_full.jsonl");
    fs::create_dir_all(child_log.parent().unwrap())?;
    fs::write(child_log,json!({"type":"PLANNER_RESPONSE","step_index":0,"status":"DONE","content":"child-only answer","created_at":"2026-11-21T01:02:03Z"}).to_string()+"\n")?;
    let body = prompt();
    for i in 0..=148 {
        let (ty, member, contents) = match i {
            0 | 88 | 148 => {
                let s = match i {
                    0 => body.as_str(),
                    88 => "last historical instruction",
                    _ => "later correction",
                };
                (
                    14,
                    19,
                    [field(2, s.as_bytes()), field(3, &field(1, s.as_bytes()))].concat(),
                )
            }
            1 => (
                127,
                143,
                CHILDREN
                    .iter()
                    .flat_map(|id| field(10, &field(1, id.as_bytes())))
                    .collect(),
            ),
            139 | 146 | 147 => {
                let id = CHILDREN[match i {
                    139 => 0,
                    146 => 1,
                    _ => 2,
                }];
                let msg = format!(
                    "[Message] sender=system content=Subagent {id} has gone idle. This was the subagent's most recent response: result {id}"
                );
                (101, 114, field(1, msg.as_bytes()))
            }
            140 => (23, 30, field(5, b"compact summary")),
            145 => (
                101,
                114,
                field(
                    1,
                    format!("[Message] sender={} content=intermediate progress", CHILDREN[1]).as_bytes(),
                ),
            ),
            _ => (15, 20, field(1, format!("reply {i}").as_bytes())),
        };
        let md = metadata(i);
        let mut payload = [int(1, ty), int(4, 3), field(5, &md), field(member, &contents)].concat();
        let task = (i == 5).then(|| [field(1, format!("{OLD}/task-7").as_bytes()), field(4, OLD.as_bytes())].concat());
        if let Some(t) = &task {
            payload.extend(field(148, t));
        }
        conn.execute(
            "INSERT INTO steps(idx,step_type,status,metadata,task_details,step_payload) VALUES(?,?,3,?,?,?)",
            params![i, ty, md, task, payload],
        )?;
    }
    for (i, data) in [
        int(2, 139),
        [int(2, 140), int(2, 147)].concat(),
        field(2, &[146, 1, 147, 1]),
    ]
    .into_iter()
    .enumerate()
    {
        conn.execute("INSERT INTO gen_metadata VALUES(?,?,0)", params![i, data])?;
    }
    // One verified neighbour places the remaining native rows. This export has
    // an extra field and a shorter model-facing body for the first input.
    let shortened = format!("{}<truncated 129210 bytes>", &body[..body.len() - 129_210]);
    fs::write(
        &log,
        format!(
            "{}\n{}\n{}\n",
            json!({"step_index":0,"type":"USER_INPUT","content":shortened,"truncated_fields":["content"],"status":"DONE"}),
            json!({"step_index":2,"type":"PLANNER_RESPONSE","content":"reply 2","status":"DONE"}),
            json!({"step_index":140,"type":"CHECKPOINT","content":"compact summary","status":"DONE"})
        ),
    )?;
    Ok((
        SessionRef::new(OLD, Harness::Antigravity, db),
        SessionRef::new(OLD, Harness::Antigravity, log),
    ))
}

fn options(out: &Path) -> RewindOptions {
    RewindOptions {
        at: CutAt::End,
        full: false,
        out_dir: Some(out.to_owned()),
        new_id: None,
        dry_run: false,
        thinking: None,
        skills_policy: SkillsPolicy::Keep,
        generator: "test".into(),
    }
}

#[test]
fn antigravity_uncompact_1516_cold_successor_and_field_coverage() -> Result<()> {
    // #tooth: a system step or a known sender alone cannot identify a settlement.
    let world = crate::test_world("antigravity-uncompact");
    let (db, log) = fixture(&world.base.join("source"))?;
    let adapter = crate::harness::rootless(Harness::Antigravity).unwrap();
    let s = adapter.parse(&log)?;
    assert_eq!(s.messages[0].text().as_deref(), Some(prompt().as_str()));
    let out = world.base.join("successor");
    let result = rewind(&log, &options(&out), true, None)?;
    assert_eq!(result.selected_steps, (0..140).chain([146, 147]).collect::<Vec<_>>());
    let p = result.recovery.unwrap();
    assert_eq!(p.excluded_prompts.len(), 1);
    assert_eq!(p.excluded_prompts[0].record_id.as_deref(), Some("148"));
    assert_eq!(p.recovered_children.len(), 1);
    assert_eq!(p.recovered_children[0].text, "child-only answer");
    assert_eq!((result.generations_kept, result.generations_omitted), (2, 1));
    let dst = result.new_path.unwrap();
    let next = adapter.parse(&SessionRef::new(&result.new_id, Harness::Antigravity, &dst))?;
    assert_eq!(next.messages[0].text(), Some(prompt()));
    assert_eq!(next.messages.len(), 142);
    assert!(!next.messages.iter().any(|m| m.kind == MessageKind::CompactionBoundary));
    let conn = Connection::open(&dst)?;
    for i in [0, 139, 140, 141] {
        let (md, b): (Vec<u8>, Vec<u8>) =
            conn.query_row("SELECT metadata,step_payload FROM steps WHERE idx=?", [i], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        let expected = proto::Reference {
            trajectory: &result.trajectory_id,
            step: i,
            turn: if i == 0 {
                0
            } else if i < 140 {
                1
            } else {
                2
            },
            cascade: &result.new_id,
        };
        expected.verify(&md)?;
        expected.verify(&proto::nested(&b, 5)?)?;
    }
    let packed: Vec<u8> = conn.query_row("SELECT data FROM gen_metadata WHERE idx=1", [], |r| r.get(0))?;
    assert_eq!(packed, field(2, &[140, 1, 141, 1]));
    let task: Vec<u8> = conn.query_row("SELECT task_details FROM steps WHERE idx=5", [], |r| r.get(0))?;
    assert_eq!(proto::nested(&task, 1)?, format!("{}/task-7", result.new_id).as_bytes());
    assert_eq!(proto::nested(&task, 4)?, OLD.as_bytes());
    let summary = Connection::open(out.join("conversation_summaries.db"))?;
    let raw: Vec<u8> = summary.query_row(
        "SELECT raw_summary FROM conversation_summaries WHERE step_count=142 AND last_user_input_step_index=88
         AND status='CASCADE_RUN_STATUS_IDLE' AND conversation_id=?",
        [&result.new_id],
        |r| r.get(0),
    )?;
    assert_eq!(
        raw,
        [
            int(2, 142),
            field(4, result.trajectory_id.as_bytes()),
            int(5, 1),
            int(16, 88)
        ]
        .concat()
    );
    let brain = out.join("brain").join(&result.new_id);
    assert!(!brain.join(".system_generated/logs").exists());
    assert_eq!(fs::read(brain.join("scratch/notes.txt"))?, b"source artifact");
    assert!(out
        .join("annotations")
        .join(format!("{}.pbtxt", result.new_id))
        .exists());
    // Installing another successor preserves the existing summary and files.
    let mut second = options(&out);
    second.new_id = Some("99999999-1111-4111-8111-111111111111".into());
    let another = rewind(&db, &second, true, None)?;
    assert_ne!(another.new_path.as_ref(), Some(&dst));
    assert_eq!(
        summary.query_row("SELECT count(*) FROM conversation_summaries", [], |r| r
            .get::<_, i64>(0))?,
        2
    );
    assert!(rewind(&db, &second, true, None).is_err());
    assert!(another.new_path.unwrap().is_file());
    // Only the side metadata is corrupt. The usual decoder prefers the intact
    // embedded copy; native publication must still refuse the entire staged root.
    let source = Connection::open(&db.path)?;
    source.execute("UPDATE steps SET metadata=? WHERE idx=139", [metadata(138)])?;
    let bad = world.base.join("refused");
    assert!(rewind(&db, &options(&bad), true, None).is_err());
    assert!(!bad.exists());
    // A cleared companion cannot shorten a body retained by the export.
    source.execute("UPDATE steps SET status=5,step_payload=NULL WHERE idx=0", [])?;
    let exported = adapter.parse(&log)?;
    assert!(exported.messages[0]
        .text()
        .unwrap()
        .ends_with("<truncated 129210 bytes>"));
    Ok(())
}

#[test]
fn cold_rewind_copies_executor_row_zero_and_strips_parent_references() -> Result<()> {
    // #tooth: executor_metadata idx=0 carries model/agent configuration and must survive verbatim,
    // while idx>0, parent_references, battle_mode_infos, and blob field 5 are omitted for standalone roots.
    let world = crate::test_world("antigravity-cold-exec-parent");
    let (db, _) = fixture(&world.base.join("source"))?;
    {
        let conn = Connection::open(&db.path)?;
        conn.execute(
            "INSERT INTO executor_metadata VALUES(0, ?1), (1, ?2)",
            params![b"exec-row-0-config".as_slice(), b"exec-row-1-stale".as_slice()],
        )?;
        conn.execute(
            "INSERT INTO parent_references VALUES(0, ?1)",
            params![b"parent-link".as_slice()],
        )?;
        conn.execute(
            "INSERT INTO battle_mode_infos VALUES(0, ?1)",
            params![b"battle-row".as_slice()],
        )?;
        let ws_uri = field(1, b"file:///tmp/space%20dir");
        let parent_ref = field(1, b"99999999-9999-4999-8999-999999999999");
        let blob = [
            field(1, &ws_uri),
            field(5, &parent_ref),
            field(6, b"99999999-9999-4999-8999-999999999999"),
        ]
        .concat();
        conn.execute("UPDATE trajectory_metadata_blob SET data=?1 WHERE id='main'", [blob])?;
    }
    let out = world.base.join("standalone");
    let res = rewind(&db, &options(&out), true, None)?;
    assert_eq!(res.executor_rows_kept, 1);
    assert_eq!(res.executor_rows_omitted, 1);
    assert_eq!(res.parent_references_omitted, 1);
    assert_eq!(res.battle_mode_rows_omitted, 1);
    assert_eq!(res.warnings.len(), 2, "{:?}", res.warnings);

    let dst = Connection::open(res.new_path.as_ref().unwrap())?;
    let exec_rows: Vec<(i64, Vec<u8>)> = dst
        .prepare("SELECT idx, data FROM executor_metadata ORDER BY idx")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(exec_rows, vec![(0, b"exec-row-0-config".to_vec())]);
    assert_eq!(
        dst.query_row("SELECT COUNT(*) FROM parent_references", [], |r| r.get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        dst.query_row("SELECT COUNT(*) FROM battle_mode_infos", [], |r| r.get::<_, i64>(0))?,
        0
    );
    let rewritten_blob: Vec<u8> =
        dst.query_row("SELECT data FROM trajectory_metadata_blob WHERE id='main'", [], |r| {
            r.get(0)
        })?;
    assert!(
        proto::nested(&rewritten_blob, 5).is_err(),
        "field 5 parent ref must be stripped"
    );
    assert_eq!(proto::nested(&rewritten_blob, 6)?, res.new_id.as_bytes());
    Ok(())
}

#[test]
fn cold_rewind_n2_excludes_build_trees_and_enforces_closure_budget() -> Result<()> {
    // #tooth: multi-gigabyte scratch/target-* Cargo dirs, nested worktrees, .git, CACHEDIR.TAG caches,
    // and files exceeding the closure budget must never be copied into the cold successor's brain dir.
    let world = crate::test_world("antigravity-cold-n2-budget");
    let src_root = world.base.join("source");
    let (db, _) = fixture(&src_root)?;
    let brain = src_root.join("brain").join(OLD);

    fs::create_dir_all(brain.join("scratch/target-wt2/debug/deps"))?;
    fs::write(
        brain.join("scratch/target-wt2/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )?;
    fs::write(
        brain.join("scratch/target-wt2/debug/deps/libgiant.rlib"),
        vec![b'R'; 32_768],
    )?;

    fs::create_dir_all(brain.join("scratch/custom_cache"))?;
    fs::write(
        brain.join("scratch/custom_cache/CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55\n",
    )?;
    fs::write(brain.join("scratch/custom_cache/blob.bin"), vec![b'C'; 4096])?;

    fs::create_dir_all(brain.join("scratch/worktrees/exp/.git"))?;
    fs::write(brain.join("scratch/worktrees/exp/main.rs"), b"fn main() {}")?;

    fs::create_dir_all(brain.join(".git"))?;
    fs::write(brain.join(".git/HEAD"), b"ref: refs/heads/main\n")?;

    fs::write(brain.join("scratch/over_budget.bin"), vec![b'B'; 4096])?;

    let out = world.base.join("bounded");
    let res = rewind_with_closure_budget(&db, &options(&out), true, None, 1024)?;
    let dst_brain = out.join("brain").join(&res.new_id);
    assert_eq!(fs::read(dst_brain.join("scratch/notes.txt"))?, b"source artifact");
    assert_eq!(res.closure_bytes, "source artifact".len() as u64);
    assert!(!dst_brain.join("scratch/target-wt2").exists());
    assert!(!dst_brain.join("scratch/custom_cache").exists());
    assert!(!dst_brain.join("scratch/worktrees").exists());
    assert!(!dst_brain.join(".git").exists());
    assert!(!dst_brain.join("scratch/over_budget.bin").exists());

    let omitted_paths: BTreeMap<&str, (Option<u64>, &str)> = res
        .uncaptured
        .iter()
        .map(|u| (u.path.as_str(), (u.bytes, u.why)))
        .collect();
    assert!(omitted_paths.contains_key(".git"), "{omitted_paths:?}");
    assert!(omitted_paths.contains_key("scratch/target-wt2"), "{omitted_paths:?}");
    assert!(omitted_paths.contains_key("scratch/custom_cache"), "{omitted_paths:?}");
    assert!(omitted_paths.contains_key("scratch/worktrees"), "{omitted_paths:?}");
    assert_eq!(
        omitted_paths.get("scratch/over_budget.bin"),
        Some(&(Some(4096), "exceeds brain closure byte budget"))
    );
    Ok(())
}

#[test]
fn cold_rewind_offline_root_without_out_and_live_or_locked_refusal() -> Result<()> {
    // #tooth: omitting --out publishes into the offline source root when no LS env is set,
    // percent-encodes workspace_uris spaces, and refuses active LS env or a locked summary DB.
    let env_guard = crate::env::Env::enter_isolated(Vec::new(), Vec::new());
    env_guard.remove("ANTIGRAVITY_LS_ADDRESS");
    env_guard.remove("ANTIGRAVITY_CSRF_TOKEN");

    let world = crate::test_world("antigravity-cold-offline-root");
    let src_root = world.base.join("app_root");
    let (db, _) = fixture(&src_root)?;
    let space_ws = world.base.join("work dir#1");
    fs::create_dir_all(&space_ws)?;
    {
        let conn = Connection::open(&db.path)?;
        let ws_uri = super::super::path_to_file_uri(&space_ws);
        assert!(ws_uri.contains("work%20dir%231"), "{ws_uri}");
        let blob = [field(1, &field(1, ws_uri.as_bytes())), field(6, OLD.as_bytes())].concat();
        conn.execute("UPDATE trajectory_metadata_blob SET data=?1 WHERE id='main'", [blob])?;
    }

    let mut in_place_opts = options(&src_root);
    in_place_opts.out_dir = None;
    let res = rewind(&db, &in_place_opts, true, None)?;
    let written = res.new_path.as_ref().unwrap();
    assert_eq!(
        written,
        &src_root.join("conversations").join(format!("{}.db", res.new_id))
    );
    assert!(written.is_file());

    let sum_conn = Connection::open(src_root.join("conversation_summaries.db"))?;
    let uris_json: String = sum_conn.query_row(
        "SELECT workspace_uris FROM conversation_summaries WHERE conversation_id=?1",
        [&res.new_id],
        |r| r.get(0),
    )?;
    assert!(uris_json.contains("work%20dir%231"), "{uris_json}");
    drop(sum_conn);

    // Live LS environment refuses implicit root writes.
    env_guard.set("ANTIGRAVITY_LS_ADDRESS", "127.0.0.1:42424");
    let err = rewind(&db, &in_place_opts, true, None)
        .err()
        .expect("live LS must refuse");
    assert!(
        err.to_string().contains("active Antigravity language server"),
        "{err:#}"
    );
    env_guard.remove("ANTIGRAVITY_LS_ADDRESS");

    // Locked conversation_summaries.db refuses immediately without hanging.
    let mut lock_conn = Connection::open(src_root.join("conversation_summaries.db"))?;
    let hold_tx = lock_conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let err = rewind(&db, &in_place_opts, true, None)
        .err()
        .expect("locked DB must refuse");
    assert!(err.to_string().contains("locked by another writer"), "{err:#}");
    drop(hold_tx);
    Ok(())
}
