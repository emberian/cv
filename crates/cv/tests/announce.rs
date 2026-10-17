//! The behaviour contract behind docs/INTERFACE-V2.md §2, "blocking is opt-in and announced":
//! every command `cv schema --commands --json` lists, run with stdout piped and stdin from
//! /dev/null, exits within 5 s — except the two daemons (`serve`, `watch`), which must have said
//! on stderr within 1 s what they are doing. Then the follow/for flags and the terminal-on-stdin
//! readers, case by case. An agent ran `cv scry` once with no idea what it did; it printed nothing
//! and waited forever (2026-10-18). This file is what makes that impossible again.

use cv_testkit::{claude, World};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A follower or daemon has this long after spawning to say what it is waiting for.
const ANNOUNCE: Duration = Duration::from_secs(1);
/// Everything else has this long to exit.
const BUDGET: Duration = Duration::from_secs(5);
/// The commands that keep running on a pipe by design.
const DAEMONS: [&str; 2] = ["serve", "watch"];

/// One `cv` child with its stderr streamed line by line and its stdout drained on the side (a full
/// pipe must never be what stops a command). Killed on drop: it is this test's own child.
struct Run {
    child: Child,
    stderr: mpsc::Receiver<String>,
    stdout: Option<std::thread::JoinHandle<String>>,
    started: Instant,
}

impl Run {
    fn spawn(w: &World, argv: &[String], stdin: Stdio) -> Run {
        let mut child = w
            .cv_command()
            .args(argv)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("cv {argv:?} did not spawn: {e}"));
        let started = Instant::now();
        let err = child.stderr.take().unwrap();
        let (tx, stderr) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let out = child.stdout.take().unwrap();
        let stdout = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = BufReader::new(out).read_to_string(&mut s);
            s
        });
        Run {
            child,
            stderr,
            stdout: Some(stdout),
            started,
        }
    }

    /// The first stderr line, when one arrives within `within` of the spawn.
    fn announced(&self, within: Duration) -> Option<String> {
        let deadline = self.started + within;
        self.stderr
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
    }

    /// The exit status, when the process exits within `within` of the spawn.
    fn exited(&mut self, within: Duration) -> Option<ExitStatus> {
        let deadline = self.started + within;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Every stderr line still queued (the process has exited or been killed).
    fn rest_of_stderr(&self) -> String {
        self.stderr.try_iter().collect::<Vec<_>>().join("\n")
    }

    fn stdout(&mut self) -> String {
        self.stdout.take().map(|t| t.join().unwrap()).unwrap_or_default()
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The command words plus a value for every required argument (the first allowed value, else
/// `1`/`x`), so the command gets past clap; `serve` binds a free port.
fn argv_for(cmd: &Value) -> Vec<String> {
    let mut argv: Vec<String> = cmd["name"].as_str().unwrap().split(' ').map(String::from).collect();
    let mut options = Vec::new();
    for a in cmd["args"].as_array().unwrap() {
        if a["required"] != Value::Bool(true) {
            continue;
        }
        let value = a["possible_values"]
            .as_array()
            .and_then(|p| p.first())
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| {
                if matches!(a["value_type"].as_str(), Some("integer" | "number")) {
                    "1".into()
                } else {
                    "x".into()
                }
            });
        match a["kind"].as_str() {
            Some("positional") => argv.push(value),
            Some("option") => options.extend([format!("--{}", a["long"].as_str().unwrap()), value]),
            _ => {}
        }
    }
    argv.extend(options);
    if argv[0] == "serve" {
        argv.extend(["--port".into(), "0".into()]);
    }
    argv
}

fn strings(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|s| s.to_string()).collect()
}

fn world(tag: &str) -> World {
    let w = World::new(tag);
    w.write_session(
        "alphasess",
        &[claude::user("u1", "2026-01-01T10:00:00Z", "hello there")],
    );
    w
}

#[test]
fn every_command_on_a_pipe_exits_or_is_a_daemon_that_announced() {
    let w = world("announce-all");
    let (out, _) = w.cv_ok(&["schema", "--commands", "--json"]);
    let commands: Vec<Value> = serde_json::from_str(&out).expect("the command tree is JSON");
    assert!(commands.len() > 30, "the tree lists {} commands", commands.len());
    for cmd in &commands {
        let argv = argv_for(cmd);
        let mut run = Run::spawn(&w, &argv, Stdio::null());
        if DAEMONS.contains(&argv[0].as_str()) {
            let line = run.announced(ANNOUNCE);
            assert!(
                line.is_some(),
                "cv {argv:?} is a daemon that said nothing within {ANNOUNCE:?}"
            );
            continue;
        }
        let status = run.exited(BUDGET);
        let stderr = run.rest_of_stderr();
        let status =
            status.unwrap_or_else(|| panic!("cv {argv:?} was still running after {BUDGET:?}; stderr:\n{stderr}"));
        assert!(status.code().is_some(), "cv {argv:?} died by signal; stderr:\n{stderr}");
        assert!(!stderr.contains("panicked"), "cv {argv:?} panicked:\n{stderr}");
    }
}

#[test]
fn scry_on_a_pipe_prints_the_current_state_once_unless_told_to_follow() {
    let w = world("announce-scry");

    let mut run = Run::spawn(&w, &strings(&["scry"]), Stdio::null());
    let line = run.announced(ANNOUNCE).expect("scry says why it is not following");
    assert!(
        line.contains("current state once") && line.contains("--follow"),
        "{line}"
    );
    assert_eq!(run.exited(BUDGET).map(|s| s.code()), Some(Some(0)));
    let out = run.stdout();
    assert!(
        out.contains("alphases") && out.contains("msg"),
        "the snapshot lists the session:\n{out}"
    );

    let mut run = Run::spawn(&w, &strings(&["scry", "--for", "1s"]), Stdio::null());
    let line = run.announced(ANNOUNCE).expect("a bounded follow announces itself");
    assert!(line.contains("following live agent activity for 1s"), "{line}");
    assert_eq!(run.exited(BUDGET).map(|s| s.code()), Some(Some(0)));
    assert!(
        run.started.elapsed() >= Duration::from_secs(1),
        "--for 1s returned early"
    );

    let mut run = Run::spawn(&w, &strings(&["scry", "--follow"]), Stdio::null());
    let line = run.announced(ANNOUNCE).expect("an unbounded follow announces itself");
    assert!(line.contains("stop with Ctrl-C or --for"), "{line}");
    assert!(
        run.exited(Duration::from_millis(1500)).is_none(),
        "--follow kept following"
    );

    let (ok, code, _, err) = w.cv(&["scry", "--for", "soon"]);
    assert!(!ok && code == 2, "a bad --for is a usage error, got {code}: {err}");
    assert!(err.contains("--for"), "{err}");
}

#[test]
fn workflow_follow_is_bounded_by_for_and_announces() {
    let w = world("announce-workflow");

    let mut run = Run::spawn(
        &w,
        &strings(&["workflow", "alphasess", "wf_none", "--for", "1s"]),
        Stdio::null(),
    );
    let line = run.announced(ANNOUNCE).expect("workflow --for announces");
    assert!(line.contains("following run \"wf_none\" for 1s"), "{line}");
    assert_eq!(run.exited(BUDGET).map(|s| s.code()), Some(Some(0)));
    assert!(run.rest_of_stderr().contains("never registered"));

    let mut run = Run::spawn(
        &w,
        &strings(&["workflow", "alphasess", "wf_none", "--follow"]),
        Stdio::null(),
    );
    let line = run.announced(ANNOUNCE).expect("workflow --follow announces");
    assert!(line.contains("stop with Ctrl-C or --for"), "{line}");
    assert!(
        run.exited(Duration::from_millis(1500)).is_none(),
        "--follow kept waiting for the run"
    );
}

/// A pseudo-terminal's slave end as a child's stdin; the master is kept open by the caller so the
/// slave stays a live terminal for the child's lifetime.
#[cfg(unix)]
fn terminal_stdin() -> (Stdio, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd;
    let (mut master, mut slave) = (0, 0);
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty");
    unsafe { (Stdio::from_raw_fd(slave), std::os::fd::OwnedFd::from_raw_fd(master)) }
}

#[cfg(unix)]
#[test]
fn stdin_readers_on_a_terminal_refuse_or_announce() {
    let w = world("announce-tty");

    let (stdin, _master) = terminal_stdin();
    let mut run = Run::spawn(&w, &strings(&["hook", "compact"]), stdin);
    assert_eq!(
        run.exited(BUDGET).map(|s| s.code()),
        Some(Some(2)),
        "hook on a terminal is usage"
    );
    assert!(run.rest_of_stderr().contains("stdin is a terminal"));

    let (stdin, _master) = terminal_stdin();
    let mut run = Run::spawn(&w, &strings(&["mcp"]), stdin);
    let line = run
        .announced(ANNOUNCE)
        .expect("mcp on a terminal says what it is waiting for");
    assert!(line.contains("terminal") && line.contains("Ctrl-D"), "{line}");
    assert!(run.exited(Duration::from_millis(1500)).is_none(), "mcp keeps serving");
}
