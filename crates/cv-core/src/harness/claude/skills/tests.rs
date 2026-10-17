use super::*;
use serde_json::json;

const SID: &str = "11111111-1111-4111-8111-111111111111";
const FRESH: &str = "# fresh\n\nDo the fresh thing.";
const STALE_NOW: &str = "# stale\n\nThe rule as it reads today.";

/// `<config>/projects/-work-proj/` with user skills `fresh` and `stale` on disk.
fn config() -> (PathBuf, PathBuf) {
    let config = std::env::temp_dir().join(format!("cv-skills-test-{}", uuid::Uuid::new_v4()));
    for (name, text) in [("fresh", FRESH), ("stale", STALE_NOW)] {
        let dir = config.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\n\n{text}\n"),
        )
        .unwrap();
    }
    let project = config.join("projects").join("-work-proj");
    std::fs::create_dir_all(&project).unwrap();
    (config, project)
}

fn entry(name: &str, source: &str, text: &str) -> Value {
    json!({"name": name, "path": format!("{source}:{name}"),
           "content": format!("Base directory for this skill: /c/skills/{name}\n\n{text}")})
}

/// The record a compaction writes: a fresh user skill, a stale one, one whose SKILL.md is gone,
/// and a bundled one with no file to compare.
fn restore(sidechain: bool) -> Value {
    json!({"type": "attachment", "uuid": "r1", "parentUuid": "s1", "sessionId": SID, "cwd": "/work/proj",
           "isSidechain": sidechain,
           "attachment": {"type": "invoked_skills", "skills": [
               entry("fresh", "userSettings", FRESH),
               entry("stale", "userSettings", "# stale\n\nThe rule as it read in June."),
               entry("gone", "userSettings", "# gone"),
               entry("simplify", "bundled", "# simplify"),
           ]},
           "rendered": [{"content": "<system-reminder>old text</system-reminder>"}], "renderedRole": "system"})
}

fn session(records: &[Value]) -> ByteSource {
    let mut stage = ByteStage::new();
    for r in records {
        stage.write_all((r.to_string() + "\n").as_bytes()).unwrap();
    }
    stage.finish().unwrap()
}

fn lines(bytes: &ByteSource) -> Vec<Value> {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut bytes.reader().unwrap(), &mut text).unwrap();
    text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn names(record: &Value) -> Vec<&str> {
    record["attachment"]["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect()
}

fn run(policy: SkillsPolicy, records: &[Value], config: &Path) -> (Vec<Value>, SkillsReport) {
    let (out, report) = apply(session(records), policy, Some(config)).unwrap();
    (lines(&out), report)
}

#[test]
fn drop_stale_keeps_current_and_unverifiable_skills() {
    let (config, _) = config();
    let prompt = json!({"type": "user", "uuid": "u1", "sessionId": SID, "message": {"role": "user", "content": "hi"}});
    let (out, report) = run(SkillsPolicy::DropStale, &[prompt.clone(), restore(false)], &config);
    assert_eq!(out[0], prompt, "other records are copied as they are");
    assert_eq!(names(&out[1]), ["fresh", "simplify"]);
    assert!(out[1].get("rendered").is_none() && out[1].get("renderedRole").is_none());
    assert_eq!(out[1]["uuid"], "r1", "the record keeps its place in the chain");
    assert_eq!((report.records, report.dropped, report.kept), (1, 2, 2));
    std::fs::remove_dir_all(config).ok();
}

#[test]
fn drop_all_empties_the_record_and_keep_changes_nothing() {
    let (config, _) = config();
    let (out, report) = run(SkillsPolicy::DropAll, &[restore(false)], &config);
    assert!(names(&out[0]).is_empty());
    assert!(out[0].get("rendered").is_none());
    assert_eq!(report.dropped, 4);
    let source = session(&[restore(false)]);
    let (kept, report) = apply(session(&[restore(false)]), SkillsPolicy::Keep, Some(&config)).unwrap();
    assert_eq!(lines(&kept), lines(&source));
    assert_eq!(report.records, 0);
    std::fs::remove_dir_all(config).ok();
}

#[test]
fn sidechain_records_and_unchanged_records_stay_verbatim() {
    let (config, _) = config();
    let fresh_only = {
        let mut r = restore(false);
        r["attachment"]["skills"] = json!([entry("fresh", "userSettings", FRESH)]);
        r
    };
    let (out, report) = run(SkillsPolicy::DropStale, &[restore(true), fresh_only.clone()], &config);
    assert_eq!(out, [restore(true), fresh_only]);
    assert_eq!(report.records, 0);
    std::fs::remove_dir_all(config).ok();
}

#[test]
fn stored_text_matches_through_arguments_and_truncation() {
    let (config, _) = config();
    let with_args = entry("fresh", "userSettings", &format!("{FRESH}\n\nARGUMENTS: --fast"));
    let truncated = entry("stale", "userSettings", &format!("# stale\n\nThe rule{TRUNCATED}"));
    assert!(current(&with_args, Some(&config), None));
    assert!(current(&truncated, Some(&config), None));
    assert!(!current(
        &entry("stale", "userSettings", "# stale\n\nOther"),
        Some(&config),
        None
    ));
    assert!(
        current(&entry("stale", "userSettings", "x"), None, None),
        "no config dir: kept"
    );
    std::fs::remove_dir_all(config).ok();
}

#[test]
fn project_skills_resolve_from_the_record_cwd() {
    let root = std::env::temp_dir().join(format!("cv-skills-proj-{}", uuid::Uuid::new_v4()));
    let dir = root.join(".claude/skills/local");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(dir.join("SKILL.md"), "# local\n").unwrap();
    let e = entry("local", "projectSettings", "# local");
    assert!(current(&e, None, Some(&root.join("sub"))));
    assert!(!current(&entry("local", "projectSettings", "# old"), None, Some(&root)));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn config_dir_is_the_parent_of_projects() {
    let (config, project) = config();
    assert_eq!(
        config_dir_of(&project.join(format!("{SID}.jsonl"))),
        Some(config.clone())
    );
    assert_eq!(config_dir_of(Path::new("/tmp/out/x.jsonl")), None);
    std::fs::remove_dir_all(config).ok();
}

#[test]
fn prune_and_rewind_apply_the_policy_to_the_new_session_only() {
    let (config, project) = config();
    let src = project.join(format!("{SID}.jsonl"));
    let records = [
        json!({"type": "user", "uuid": "u0", "sessionId": SID, "cwd": "/work/proj", "isSidechain": false,
               "message": {"role": "user", "content": "go"}}),
        json!({"type": "system", "subtype": "compact_boundary", "uuid": "s1", "parentUuid": null,
               "logicalParentUuid": "u0", "sessionId": SID, "cwd": "/work/proj", "isSidechain": false}),
        restore(false),
        json!({"type": "user", "uuid": "u1", "parentUuid": "r1", "sessionId": SID, "cwd": "/work/proj",
               "isSidechain": false, "message": {"role": "user", "content": "next"}}),
    ];
    let body: String = records.iter().map(|r| r.to_string() + "\n").collect();
    std::fs::write(&src, &body).unwrap();

    let pruned = crate::prune::prune_session(
        &src,
        &crate::prune::PruneOptions {
            skills_policy: SkillsPolicy::DropStale,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(pruned.skills.dropped, 2);
    let out: Vec<Value> = std::fs::read_to_string(&pruned.new_path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(names(&out[2]), ["fresh", "simplify"]);
    assert_eq!(out[2]["sessionId"], pruned.new_id.as_str());

    let rewound = crate::rewind::rewind_session(
        &crate::ir::SessionRef::from_path(&src, crate::ir::Harness::Claude),
        &crate::rewind::RewindOptions {
            at: crate::rewind::CutAt::End,
            full: true,
            out_dir: None,
            new_id: None,
            dry_run: false,
            thinking: Some(crate::emit::ThinkingMode::Native),
            skills_policy: SkillsPolicy::DropAll,
            generator: "cv-test".into(),
        },
    )
    .unwrap();
    assert_eq!(rewound.skills.dropped, 4);
    let text = std::fs::read_to_string(&rewound.new_path).unwrap();
    assert_eq!(rewound.bytes_written, text.len() as u64);
    let restored: Value = serde_json::from_str(text.lines().nth(2).unwrap()).unwrap();
    assert!(names(&restored).is_empty() && restored.get("rendered").is_none());

    assert_eq!(
        std::fs::read_to_string(&src).unwrap(),
        body,
        "the source is never touched"
    );
    std::fs::remove_dir_all(config).ok();
}
