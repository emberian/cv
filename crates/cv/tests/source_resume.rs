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
    assert_eq!(
        plan["env"]["CLAUDE_CONFIG_DIR"],
        config.canonicalize().unwrap().to_str().unwrap()
    );
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
            config.canonicalize().unwrap().to_str().unwrap(),
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
    for verb in ["doctor", "resume"] {
        let (_, code, _, err) = w.cv(&[verb, "--harness", "hermes", path.to_str().unwrap(), "--json"]);
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
