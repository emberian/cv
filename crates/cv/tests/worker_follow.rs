//! Worker following exercises the real CLI against a writer changing an isolated session.
use cv_testkit::{claude, World};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SESSION: &str = "follow-parent";
const AGENT: &str = "worker-1";
const TS: &str = "2026-11-01T00:00:00Z";

fn running_world() -> World {
    let w = World::new("lanes-follow");
    w.write_session(SESSION, &[claude::user(SESSION, "p1", TS, "start a worker")]);
    w.write_agent(
        SESSION,
        AGENT,
        &json!({"agentType":"general-purpose", "description":"build it"}),
        &[
            claude::user(SESSION, "w1", TS, "build it"),
            claude::assistant(
                "w2",
                TS,
                "test",
                json!([claude::tool_use("call1", "Bash", json!({"command":"make"}))]),
            ),
        ],
    );
    w
}

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn follow(w: &World, args: &[&str]) -> (Running, mpsc::Receiver<String>) {
    let mut child = w
        .cv_command()
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    (Running(child), rx)
}

fn row(rx: &mpsc::Receiver<String>) -> Value {
    let text = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("follower must flush each row promptly");
    serde_json::from_str(&text).unwrap()
}

fn wait_success(child: &mut Running) {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            return;
        }
        assert!(Instant::now() < deadline, "--for did not end the follower");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn append(path: &std::path::Path, rows: &[Value]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, row).unwrap();
        bytes.push(b'\n');
    }
    file.write_all(&bytes).unwrap();
}

#[test]
fn lanes_follow_reports_initial_and_completed_rows_without_idle_duplicates() {
    let w = running_world();
    let (mut child, rx) = follow(
        &w,
        &["lanes", SESSION, "--running", "--follow", "--for", "4s", "--json"],
    );
    let initial = row(&rx);
    assert_eq!(initial["agent_id"], AGENT);
    assert_eq!(initial["status"], "running");
    assert_eq!(initial["parent_id"], SESSION);
    assert_eq!(initial["parent_harness"], "claude");
    assert_eq!(initial["parent_path"], w.transcript(SESSION).to_str().unwrap());
    assert!(
        matches!(
            rx.recv_timeout(Duration::from_millis(1200)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "an idle poll must not repeat the row"
    );

    let agent = w
        .proj
        .join(SESSION)
        .join("subagents")
        .join(format!("agent-{AGENT}.jsonl"));
    append(
        &agent,
        &[
            claude::tool_result("w3", "2026-11-01T00:01:00Z", "call1", "built"),
            claude::assistant(
                "w4",
                "2026-11-01T00:01:01Z",
                "test",
                json!([{"type":"text","text":"Build completed."}]),
            ),
        ],
    );
    append(
        &w.transcript(SESSION),
        &[claude::notification("2026-11-01T00:01:02Z", AGENT, "completed")],
    );
    let mut changed = row(&rx);
    // A poll can land between the child result and its parent notification.
    if changed["status"] != "completed" {
        changed = row(&rx);
    }
    assert_eq!(
        changed["status"], "completed",
        "--running still reports a followed worker's final transition"
    );
    assert_eq!(changed["last_text"], "Build completed.");
    wait_success(&mut child);
    assert!(rx.try_iter().next().is_none(), "unchanged completion must not repeat");
}

#[test]
fn lanes_for_zero_prints_once_and_normal_json_stays_an_array() {
    let w = running_world();
    let (mut child, rx) = follow(&w, &["--frozen", "lanes", SESSION, "--for", "0s", "--json"]);
    assert_eq!(row(&rx)["parent_id"], SESSION);
    wait_success(&mut child);
    assert!(rx.try_iter().next().is_none());
    let (out, _) = w.cv_ok(&["lanes", SESSION, "--json"]);
    let normal: Value = serde_json::from_str(&out).unwrap();
    assert!(normal.is_array());
    assert!(normal[0].get("parent_id").is_none());
    let (_, code, _, _) = w.cv(&["lanes", "--recent", "2", "--follow"]);
    assert_eq!(code, 2);
    let (_, code, _, _) = w.cv(&["lanes", SESSION, "--for", "soon"]);
    assert_eq!(code, 2);
}

#[cfg(unix)]
#[test]
fn lanes_follow_stops_on_sigint() {
    use std::os::unix::process::ExitStatusExt;
    let w = running_world();
    let (mut child, rx) = follow(&w, &["lanes", SESSION, "--follow", "--json"]);
    assert_eq!(row(&rx)["agent_id"], AGENT);
    // This is the child created above, not a discovered or unrelated process.
    let pid = i32::try_from(child.0.id()).unwrap();
    assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert_eq!(status.signal(), Some(libc::SIGINT));
            break;
        }
        assert!(Instant::now() < deadline, "SIGINT did not stop the follower");
        std::thread::sleep(Duration::from_millis(20));
    }
}
