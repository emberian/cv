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
