//! Exact transcript paths must survive through diagnosis and native resume planning.
use cv_testkit::{claude, jsonl, World};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

const ID: &str = "11111111-2222-4333-8444-555555555555";
const TS: &str = "2026-11-01T00:00:00Z";

fn write_claude(path: &Path, cwd: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let row = claude::with(claude::user(ID, "u1", TS, text), json!({"cwd":cwd}));
    fs::write(path, jsonl(&[row])).unwrap();
}

#[test]
fn exact_source_doctor_and_resume_select_the_requested_claude_root() {
    let w = World::new("resume-source-root");
    w.write_session(ID, &[claude::user(ID, "u1", TS, "short")]);
    let config = w.base.join("other 'profile $home");
    let transcript = config.join("projects/-project").join(format!("{ID}.jsonl"));
    write_claude(&transcript, &w.base, &"long prompt ".repeat(1000));
    let source = format!("claude:{}", transcript.display());
    let (out, _) = w.cv_ok(&["doctor", &source, "--json"]);
    let selected: Value = serde_json::from_str(&out).unwrap();
    let (out, _) = w.cv_ok(&[
        "doctor",
        "--harness",
        "claude",
        w.transcript(ID).to_str().unwrap(),
        "--json",
    ]);
    let default: Value = serde_json::from_str(&out).unwrap();
    assert!(
        selected["conversational"]["total"].as_u64().unwrap() > default["conversational"]["total"].as_u64().unwrap()
    );
    assert!(selected.get("lineage").is_none());
    let (out, _) = w.cv_ok(&["resume", &source, "--json"]);
    let plan: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(plan["id"], ID);
    assert_eq!(plan["args"], json!(["--resume", ID]));
    // The directory keeps the spelling it was selected with: Claude keys credentials on it.
    assert_eq!(plan["env"]["CLAUDE_CONFIG_DIR"], config.to_str().unwrap());
    assert_eq!(plan["unset_env"], json!([]));
    assert_eq!(plan["cwd"], w.base.to_str().unwrap());
    assert_eq!(plan["path"], transcript.to_str().unwrap());
    let (out, _) = w.cv_ok(&["resume", "--harness", "claude", transcript.to_str().unwrap()]);
    assert!(out.contains("env CLAUDE_CONFIG_DIR='"), "{out}");
    assert!(out.contains("'\\''profile $home'"), "{out}");
}

#[test]
fn exact_source_resume_reads_codex_native_id_and_home() {
    let w = World::new("resume-codex-source");
    let home = w.base.join("other codex");
    for dir in ["sessions/2026/11/01", "archived_sessions"] {
        let path = home.join(dir).join(format!("rollout-2026-11-01T00-00-00-{ID}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            jsonl(&[json!({"timestamp":TS,"type":"session_meta","payload":{"id":ID,"cwd":w.base}})]),
        )
        .unwrap();
        let (out, _) = w.cv_ok(&["resume", "--harness", "codex", path.to_str().unwrap(), "--json"]);
        let plan: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(plan["args"], json!(["resume", ID]));
        assert_eq!(
            plan["env"]["CODEX_HOME"],
            home.canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(plan["cwd"], w.base.to_str().unwrap());
    }
}

#[test]
fn exact_source_resume_refuses_export_and_child_paths() {
    let w = World::new("resume-unaddressable-source");
    for relative in [
        "export.jsonl",
        "profile/projects/-project/parent/subagents/agent-child.jsonl",
        "profile/projects/-project/agent-legacy-child.jsonl",
        "profile/projects/-project/session.flat.jsonl",
    ] {
        let path = w.base.join(relative);
        write_claude(&path, &w.base, "hello");
        let (_, code, _, err) = w.cv(&["resume", "--harness", "claude", path.to_str().unwrap(), "--json"]);
        assert_eq!(code, 2, "{err}");
        assert!(
            err.contains("cannot safely resume claude") && err.contains("original harness/profile"),
            "{err}"
        );
    }
}

#[cfg(unix)]
#[test]
fn exact_source_resume_launch_passes_selected_home_and_cwd() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new("resume-launch-source");
    let config = w.base.join("selected profile");
    let path = config.join("projects/-project").join(format!("{ID}.jsonl"));
    write_claude(&path, &w.base, "hello");
    let bin = w.base.join("bin");
    fs::create_dir(&bin).unwrap();
    let program = bin.join("claude");
    fs::write(
        &program,
        "#!/bin/sh\nprintf '%s\\n' \"$CLAUDE_CONFIG_DIR\" \"$PWD\" \"$@\" > \"$CV_LAUNCH_CAPTURE\"\n",
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let capture = w.base.join("launch.txt");
    let result = w
        .cv_command()
        .args(["resume", "--harness", "claude", path.to_str().unwrap(), "--launch"])
        .env("PATH", &bin)
        .env("CV_LAUNCH_CAPTURE", &capture)
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let text = fs::read_to_string(capture).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(
        lines,
        [
            config.to_str().unwrap(),
            w.base.canonicalize().unwrap().to_str().unwrap(),
            "--resume",
            ID
        ]
    );
}

#[test]
fn exact_source_shared_database_requires_a_native_session_id() {
    let w = World::new("resume-shared-source");
    let path = w.base.join("state.db");
    fs::write(&path, []).unwrap();
    let selector = format!("hermes:{}", path.display());
    for args in [
        vec!["doctor", &selector, "--json"],
        vec!["resume", &selector, "--json"],
        vec!["show", &selector],
        vec!["show", &selector, "--kind", "overview"],
        vec!["pack", "--session", &selector],
    ] {
        let (_, code, _, err) = w.cv(&args);
        assert_eq!(code, 2, "{err}");
        assert!(
            err.contains("shared hermes store") && err.contains("hermes:<session-id>"),
            "{err}"
        );
    }
}

#[test]
fn exact_source_resume_refuses_codex_filenames_native_lookup_cannot_address() {
    let w = World::new("resume-codex-native-filename");
    let day = w.base.join("profile/sessions/2026/11/01");
    fs::create_dir_all(&day).unwrap();
    for name in [
        "renamed.jsonl".to_string(),
        format!("rollout-2026-11-01T00-00-00Z-{ID}.jsonl"),
        "rollout-2026-11-01T00-00-00-66666666-7777-4888-8999-aaaaaaaaaaaa.jsonl".to_string(),
        format!("rollout-2026-11-01T00-00-00-{ID}.jsonl.tmp"),
    ] {
        let path = day.join(name);
        fs::write(
            &path,
            jsonl(&[json!({"timestamp":TS,"type":"session_meta","payload":{"id":ID,"cwd":w.base}})]),
        )
        .unwrap();
        let (_, code, _, err) = w.cv(&["resume", "--harness", "codex", path.to_str().unwrap(), "--json"]);
        assert_eq!(code, 2, "{err}");
        assert!(
            err.contains("native filename does not address session") && err.contains(ID),
            "{err}"
        );
    }
}

/// Claude Code reads another global config and keychain entry whenever `CLAUDE_CONFIG_DIR` is
/// set, even to `~/.claude`, so the default profile is never named.
#[test]
fn default_claude_profile_is_resumed_without_naming_its_directory() {
    let w = World::new("resume-default-profile");
    w.write_session(
        ID,
        &[claude::with(claude::user(ID, "u1", TS, "hello"), json!({"cwd":w.base}))],
    );
    let path = w.transcript(ID);
    for selector in [ID.to_string(), format!("claude:{}", path.display())] {
        let (out, _) = w.cv_ok(&["resume", &selector, "--json"]);
        let plan: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(plan["env"], json!({}), "{selector}");
        assert_eq!(plan["unset_env"], json!([]), "{selector}");
        let (out, _) = w.cv_ok(&["resume", &selector]);
        assert!(out.ends_with(&format!("\nclaude --resume {ID}\n")), "{out}");
    }

    // From an environment that selects another profile, the default is reached by unsetting.
    let other = w.base.join("other");
    fs::create_dir_all(other.join("projects")).unwrap();
    let env = [("CLAUDE_CONFIG_DIR", other.to_str().unwrap())];
    let (ok, _, out, err) = w.cv_env(
        &["resume", path.to_str().unwrap(), "--harness", "claude", "--json"],
        &env,
    );
    assert!(ok, "{err}");
    let plan: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(plan["env"], json!({}));
    assert_eq!(plan["unset_env"], json!(["CLAUDE_CONFIG_DIR"]));
    let (ok, _, out, err) = w.cv_env(&["resume", path.to_str().unwrap(), "--harness", "claude"], &env);
    assert!(ok, "{err}");
    assert!(
        out.contains(&format!("env -u CLAUDE_CONFIG_DIR claude --resume {ID}")),
        "{out}"
    );
}

#[cfg(unix)]
#[test]
fn ambient_claude_profile_with_shared_projects_keeps_its_own_identity() {
    // `$CLAUDE_CONFIG_DIR/projects` may be a link to another profile's history. The caller's
    // profile already reads the transcript, so its credentials stay selected.
    let w = World::new("resume-linked-projects");
    w.write_session(
        ID,
        &[claude::with(claude::user(ID, "u1", TS, "hello"), json!({"cwd":w.base}))],
    );
    let profile = w.base.join("work");
    fs::create_dir_all(&profile).unwrap();
    std::os::unix::fs::symlink(w.home.join(".claude/projects"), profile.join("projects")).unwrap();
    let env = [("CLAUDE_CONFIG_DIR", profile.to_str().unwrap())];
    let (ok, _, out, err) = w.cv_env(&["resume", ID, "--json"], &env);
    assert!(ok, "{err}");
    let plan: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(plan["env"], json!({}));
    assert_eq!(plan["unset_env"], json!([]));

    // Selected from outside that profile, through the link: the linked profile is named as spelled.
    let linked = profile.join("projects/-work-proj").join(format!("{ID}.jsonl"));
    let (out, _) = w.cv_ok(&["resume", "--harness", "claude", linked.to_str().unwrap(), "--json"]);
    let plan: Value = serde_json::from_str(&out).unwrap();
    // Reached through the home default too, which wins: no variable is needed.
    assert_eq!(plan["env"], json!({}));
}

#[test]
fn pi_resume_selects_custom_agent_dir_unsets_override_for_default_and_refuses_loose_files() {
    let w = World::new("resume-pi-source");
    let default_path = w.home.join(".pi/agent/sessions/proj").join(format!("{ID}.jsonl"));
    fs::create_dir_all(default_path.parent().unwrap()).unwrap();
    let rows = jsonl(&[
        json!({"type":"session","version":3,"id":ID,"cwd":w.base,"timestamp":TS}),
        json!({"type":"message","id":"m1","parentId":null,"message":{"role":"user","content":"hello pi"}}),
    ]);
    fs::write(&default_path, &rows).unwrap();

    // Default ~/.pi/agent/sessions needs no env override.
    let (out, _) = w.cv_ok(&["resume", &format!("pi:{ID}"), "--json"]);
    let plan: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(plan["program"], "pi");
    assert_eq!(plan["args"], json!(["--session", ID]));
    assert_eq!(plan["env"], json!({}));
    assert_eq!(plan["unset_env"], json!([]));
    assert_eq!(plan["cwd"], w.base.to_str().unwrap());

    // Custom agent dir sets PI_CODING_AGENT_DIR and extracts native header id from timestamped filename.
    let custom_agent = w.base.join("custom pi agent");
    let custom_path = custom_agent
        .join("sessions/proj")
        .join(format!("2026-11-01T00-00-00_{ID}.jsonl"));
    fs::create_dir_all(custom_path.parent().unwrap()).unwrap();
    fs::write(&custom_path, &rows).unwrap();
    let (out, _) = w.cv_ok(&["resume", "--harness", "pi", custom_path.to_str().unwrap(), "--json"]);
    let plan: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(plan["id"], ID);
    assert_eq!(plan["program"], "pi");
    assert_eq!(plan["args"], json!(["--session", ID]));
    assert_eq!(plan["env"]["PI_CODING_AGENT_DIR"], custom_agent.to_str().unwrap());
    assert_eq!(plan["unset_env"], json!([]));

    // With ambient PI_CODING_AGENT_DIR pointing at custom_agent, resuming default_path unsets it.
    let env = [("PI_CODING_AGENT_DIR", custom_agent.to_str().unwrap())];
    let (ok, _, out, err) = w.cv_env(
        &["resume", "--harness", "pi", default_path.to_str().unwrap(), "--json"],
        &env,
    );
    assert!(ok, "{err}");
    let plan: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(plan["env"], json!({}));
    assert_eq!(plan["unset_env"], json!(["PI_CODING_AGENT_DIR"]));

    // Loose export file outside any <agent_dir>/sessions/ tree is refused with exit 2.
    let loose = w.base.join("export.jsonl");
    fs::write(&loose, &rows).unwrap();
    let (_, code, _, err) = w.cv(&["resume", "--harness", "pi", loose.to_str().unwrap(), "--json"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("cannot safely resume pi"), "{err}");
}
