use super::*;
use crate::ir::Harness;
use serde_json::json;

const SID: &str = "11111111-1111-4111-8111-111111111111";
const SHA: &str = "63cef473b771e07a2b5060e50373a41a27d54c0e";

fn write_jsonl(path: &Path, lines: &[Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body: String = lines.iter().map(|l| l.to_string() + "\n").collect();
    std::fs::write(path, body).unwrap();
}

fn append(path: &Path, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

fn rec(ty: &str, uuid: &str, parent: Option<&str>, message: Value) -> Value {
    json!({"type": ty, "sessionId": SID, "uuid": uuid, "parentUuid": parent, "isSidechain": false,
           "cwd": "/work/proj", "timestamp": "2026-06-16T00:00:00Z", "message": message})
}

fn bash(id: &str, cmd: &str) -> Value {
    json!({"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": cmd}}]})
}

fn result(id: &str, out: &str) -> Value {
    json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": out}]})
}

fn text(role: &str, t: &str) -> Value {
    json!({"role": role, "content": [{"type": "text", "text": t}]})
}

/// A two-compaction session shaped like the real ones (1-based source lines):
///
/// ```text
///  1 prompt               7 result "[main 63cef47]"    13 result t1
///  2 `cat` call           8 assistant a2 "landed"      14 hook attachment
///  3 decoy: sha in cat    9 boundary b2 (preserves a2) 15 result t2
///  4 boundary b1         10 summary s2                 16 assistant a5 "done"
///  5 summary s1          11 Read t1 ┐ parallel         17 prompt "next thing"
///  6 git commit (a1)     12 Read t2 ┘
/// ```
fn session(dir: &Path) -> PathBuf {
    let lines = vec![
        rec(
            "user",
            "u0",
            None,
            json!({"role": "user", "content": "land the gate fix"}),
        ),
        rec("assistant", "d0", Some("u0"), bash("toolu_d", "cat notes.txt")),
        // Mentions the sha, but `cat` is no commit — must never count as evidence.
        rec(
            "user",
            "d1",
            Some("d0"),
            result("toolu_d", &format!("todo: review {SHA}")),
        ),
        json!({"type": "system", "subtype": "compact_boundary", "sessionId": SID, "uuid": "b1",
               "parentUuid": null, "logicalParentUuid": "d1", "isSidechain": false,
               "compactMetadata": {"trigger": "auto", "preTokens": 900000}}),
        json!({"type": "user", "sessionId": SID, "uuid": "s1", "parentUuid": "b1", "isSidechain": false,
               "isCompactSummary": true, "message": {"role": "user", "content": "Summary: landing the gate fix"}}),
        rec(
            "assistant",
            "a1",
            Some("s1"),
            bash("toolu_c", "cd /work/proj && git add -A && git commit -m 'gate fix'"),
        ),
        rec(
            "user",
            "r1",
            Some("a1"),
            result("toolu_c", "[main 63cef47] gate fix\n 1 file changed"),
        ),
        rec("assistant", "a2", Some("r1"), text("assistant", "landed 63cef47")),
        json!({"type": "system", "subtype": "compact_boundary", "sessionId": SID, "uuid": "b2",
               "parentUuid": null, "logicalParentUuid": "a2", "isSidechain": false,
               "compactMetadata": {"trigger": "auto",
                   "preservedSegment": {"headUuid": "a2", "anchorUuid": "s2", "tailUuid": "a2"}}}),
        json!({"type": "user", "sessionId": SID, "uuid": "s2", "parentUuid": "b2", "isSidechain": false,
               "isCompactSummary": true, "message": {"role": "user", "content": "Summary: gate fix landed"}}),
        rec(
            "assistant",
            "t1u",
            Some("s2"),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/a"}}]}),
        ),
        rec(
            "assistant",
            "t2u",
            Some("t1u"),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t2", "name": "Read", "input": {"file_path": "/b"}}]}),
        ),
        rec("user", "t1r", Some("t2u"), result("t1", "contents of a")),
        json!({"type": "attachment", "sessionId": SID, "uuid": "h1", "parentUuid": "t1r", "isSidechain": false,
               "attachment": {"type": "hook_success", "content": "ok"}, "rendered": [{"content": "hook ok"}]}),
        rec("user", "t2r", Some("h1"), result("t2", "contents of b")),
        rec("assistant", "a5", Some("t2r"), text("assistant", "done")),
        rec(
            "user",
            "u9",
            Some("a5"),
            json!({"role": "user", "content": "next thing"}),
        ),
    ];
    let path = dir.join(format!("{SID}.jsonl"));
    write_jsonl(&path, &lines);
    path
}

fn sref(path: &Path, id: &str) -> SessionRef {
    let r = SessionRef::from_path(path, Harness::Claude);
    assert_eq!(r.id, id, "a fixture's id is its file stem");
    r
}

/// The records a real transcript ends with AFTER its last message — none of them a message, and
/// none carrying a `uuid`. The measured defect: `cv rewind --full` of a 238-record session wrote
/// 235, dropping exactly these three shapes.
fn tail_records() -> Vec<Value> {
    vec![
        json!({"type": "bridge-session", "sessionId": SID, "bridgeSessionId": "cse_1", "lastSequenceNum": 0}),
        json!({"type": "history-suppression", "sessionId": SID, "cause": "env_owner_mismatch", "ts": "2026-10-01T22:25:24.554Z"}),
        json!({"type": "cost-state", "sessionId": SID, "totalCostUSD": 0, "totalLinesAdded": 0}),
    ]
}

/// A prompt, its answer, then the tail above: two messages, five records.
fn session_tailed(dir: &Path) -> PathBuf {
    let mut lines = vec![
        rec("user", "u0", None, text("user", "land the gate fix")),
        rec("assistant", "a0", Some("u0"), text("assistant", "landed")),
    ];
    lines.extend(tail_records());
    let path = dir.join(format!("{SID}.jsonl"));
    write_jsonl(&path, &lines);
    path
}

/// The synthetic case from the measured evidence (task7336 `critical-tail`): two messages and an
/// `invoked_skills` attachment record whose `rendered` is a string (so the codec surfaces no
/// message for it). `cv show` counts 2 messages and stops at the prompt.
fn session_attachment_tail(dir: &Path) -> PathBuf {
    session_tailed_attachment_with(dir, false)
}
/// [`session_attachment_tail`] plus a trailing record with a `type` claiming a turn and no
/// `message` body.
fn session_attachment_and_bad_tail(dir: &Path) -> PathBuf {
    session_tailed_attachment_with(dir, true)
}
fn session_tailed_attachment_with(dir: &Path, bad_tail: bool) -> PathBuf {
    let mut lines = vec![
        rec("user", "u0", None, text("user", "go")),
        rec("assistant", "a0", Some("u0"), text("assistant", "done")),
        json!({"type": "attachment", "uuid": "at1", "parentUuid": "a0", "sessionId": SID,
               "cwd": "/work/proj", "isSidechain": false,
               "attachment": {"type": "invoked_skills", "skills": [
                   {"name": "fresh", "path": "userSettings:fresh", "content": "the amber key"}]},
               "rendered": "padding"}),
    ];
    if bad_tail {
        lines.push(json!({"type": "user", "sessionId": SID, "uuid": "bad1"}));
    }
    let path = dir.join(format!("{SID}.jsonl"));
    write_jsonl(&path, &lines);
    path
}

fn ty(v: &Value) -> &str {
    v["type"].as_str().unwrap()
}

/// The stream index (the `cv show --range` counting) of the message with this uuid.
fn idx_of(path: &Path, uuid: &str) -> usize {
    let s = crate::harness::claude::parse_str("x", &std::fs::read_to_string(path).unwrap(), Some(path.to_path_buf()));
    s.messages
        .iter()
        .position(|m| m.id.as_deref() == Some(uuid))
        .unwrap_or_else(|| panic!("no message {uuid}"))
}

fn opts(at: CutAt, out: &Path) -> RewindOptions {
    RewindOptions {
        at,
        full: false,
        out_dir: Some(out.to_path_buf()),
        new_id: Some("22222222-2222-4222-8222-222222222222".into()),
        dry_run: false,
        generator: "cv-test".into(),
        thinking: Some(crate::emit::ThinkingMode::Native),
        skills_policy: SkillsPolicy::Keep,
    }
}

fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn uuids(lines: &[Value]) -> Vec<&str> {
    lines
        .iter()
        .filter_map(|v| v.get("uuid").and_then(Value::as_str))
        .collect()
}

/// Walk `parentUuid` back from the last record: the chain Claude Code loads on resume. Returns the
/// uuids leaf-first, panicking on a dangling parent.
fn chain(lines: &[Value]) -> Vec<String> {
    let by: HashMap<&str, &Value> = lines
        .iter()
        .filter_map(|v| Some((v.get("uuid")?.as_str()?, v)))
        .collect();
    let mut out = Vec::new();
    let mut cur = lines.iter().rev().find(|v| v.get("uuid").is_some()).unwrap();
    loop {
        out.push(cur["uuid"].as_str().unwrap().to_string());
        match cur.get("parentUuid").and_then(Value::as_str) {
            None => return out,
            Some(p) => cur = by.get(p).unwrap_or_else(|| panic!("dangling parent {p}")),
        }
    }
}

#[test]
fn cut_at_message_index_starts_at_the_last_boundary_before_it() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = session(&dir);
    let out = dir.join("out");
    let cut = idx_of(&src, "a2");
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Message(cut), &out)).unwrap();

    // Window = boundary b1 (line 4) through a2 (line 8): the context the agent had at a2.
    assert_eq!((res.start_line, res.cut_line, res.end_line), (4, 8, 8));
    assert_eq!(res.boundary_msg_idx, Some(idx_of(&src, "b1")));
    assert_eq!(res.start_msg_idx, idx_of(&src, "b1"));
    assert!(!res.preserved_head);
    let got = read_lines(&res.new_path);
    assert_eq!(uuids(&got), ["b1", "s1", "a1", "r1", "a2"]);
    assert_eq!(res.lines_written, 5);
    // Every record carries the new id; parent links are exactly the source's.
    assert!(got
        .iter()
        .all(|v| v["sessionId"] == "22222222-2222-4222-8222-222222222222"));
    assert_eq!(chain(&got), ["a2", "r1", "a1", "s1", "b1"]);
    // The future is omitted, and said so.
    assert_eq!(res.omitted_lines, 9);
    assert_eq!(res.cwd.as_deref(), Some("/work/proj"));

    // A cut ON the later boundary's window starts at that boundary — never at an earlier one.
    let later = rewind_session(
        &sref(&src, SID),
        &RewindOptions {
            new_id: Some("33333333-3333-4333-8333-333333333333".into()),
            ..opts(CutAt::Message(idx_of(&src, "s2")), &out)
        },
    )
    .unwrap();
    assert_eq!(later.boundary_msg_idx, Some(idx_of(&src, "b2")));

    // Out of range is a clear error, not a silent clamp.
    let err = rewind_session(&sref(&src, SID), &opts(CutAt::Message(999), &dir.join("x"))).unwrap_err();
    assert!(err.to_string().contains("message(s)"), "{err}");
}

#[test]
fn cut_at_commit_sha_lands_on_the_tool_result_that_made_it() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = session(&dir);
    let out = dir.join("out");
    // The full sha resolves through the 7-digit `[main 63cef47]` the commit printed — and the
    // earlier `cat` result that merely mentions the sha is no evidence.
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Commit(SHA.into()), &out)).unwrap();
    assert_eq!(res.cut_line, 7, "{res:?}");
    assert_eq!(res.cut_msg_idx, idx_of(&src, "r1"));
    let ev = res.evidence.as_ref().unwrap();
    assert_eq!(ev.kind, EvidenceKind::Created);
    assert_eq!(ev.tool_use_id, "toolu_c");
    assert!(ev.command.contains("git commit"));
    assert_eq!(uuids(&read_lines(&res.new_path)), ["b1", "s1", "a1", "r1"]);

    // A short sha works the same way.
    let short = rewind_session(
        &sref(&src, SID),
        &RewindOptions {
            dry_run: true,
            ..opts(CutAt::Commit("63CEF47".into()), &out)
        },
    )
    .unwrap();
    assert_eq!(short.cut_line, 7);

    // No evidence → a clear failure naming the sha, nothing written.
    let none = dir.join("none");
    let err = rewind_session(&sref(&src, SID), &opts(CutAt::Commit("abcdef1234".into()), &none)).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("abcdef1234") && msg.contains("git commit"), "{msg}");
    assert!(!none.exists());
}

#[test]
fn from_compaction_takes_the_preserved_segment_and_full_takes_everything() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = session(&dir);
    let out = dir.join("out");
    let cut = idx_of(&src, "a5");
    // b2 preserved a2 (line 8, just before the boundary): the start moves back to it.
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Message(cut), &out)).unwrap();
    assert!(res.preserved_head);
    assert_eq!((res.start_line, res.cut_line), (8, 16));
    assert_eq!(res.start_msg_idx, idx_of(&src, "a2"));
    assert_eq!(uuids(&read_lines(&res.new_path))[..3], ["a2", "b2", "s2"]);

    let full = rewind_session(
        &sref(&src, SID),
        &RewindOptions {
            full: true,
            new_id: Some("44444444-4444-4444-8444-444444444444".into()),
            ..opts(CutAt::Message(cut), &out)
        },
    )
    .unwrap();
    assert_eq!(
        (full.start_line, full.start_mode, full.boundary_msg_idx),
        (1, "full", None)
    );
    assert_eq!(full.lines_written, 16);
}

/// `CutAt::End` copies bookkeeping after the last message, rewriting its session identity too.
#[test]
fn end_copies_the_records_after_the_last_message() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = session_tailed(&dir);
    let before = std::fs::read(&src).unwrap();
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::End, &dir.join("out"))).unwrap();

    let got = read_lines(&res.new_path);
    assert_eq!(
        got.iter().map(ty).collect::<Vec<_>>(),
        [
            "user",
            "assistant",
            "bridge-session",
            "history-suppression",
            "cost-state"
        ]
    );
    // The tail is the source's records, restamped and nothing else.
    assert_eq!(got[2]["bridgeSessionId"], "cse_1");
    assert_eq!(got[3]["cause"], "env_owner_mismatch");
    assert_eq!(got[4]["totalCostUSD"], 0);
    assert!(got
        .iter()
        .all(|v| v["sessionId"] == "22222222-2222-4222-8222-222222222222"));
    // Nothing was left behind: the copy runs to the end of the source as read.
    assert_eq!((res.cut_line, res.end_line, res.lines_written), (2, 5, 5));
    assert_eq!((res.omitted_lines, res.omitted_bytes), (0, 0));
    assert_eq!(std::fs::read(&src).unwrap(), before, "the source must never change");
}

/// An explicit cut ends at its message record, omitting later bookkeeping.
#[test]
fn an_explicit_cut_still_ends_at_its_message_record() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = session_tailed(&dir);
    let cut = idx_of(&src, "a0"); // the last message
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Message(cut), &dir.join("out"))).unwrap();

    let got = read_lines(&res.new_path);
    assert_eq!(got.iter().map(ty).collect::<Vec<_>>(), ["user", "assistant"]);
    assert_eq!((res.cut_line, res.end_line), (2, 2));
    assert_eq!(res.omitted_lines, 3);
}

/// A bodyless native user record is bookkeeping and survives a full byte copy.
#[test]
fn a_native_copy_carries_a_bodyless_turn_record_verbatim() {
    // The Claude codec carries a `user` record with no `message` as bookkeeping, so a byte copy
    // keeps it and the attachment after it rides along to EOF.
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = session_attachment_and_bad_tail(&dir);
    let out = dir.join("out");
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::End, &out)).unwrap();
    let copied = std::fs::read_to_string(out.join(format!("{}.jsonl", res.new_id))).unwrap();
    assert!(copied.contains("\"uuid\":\"bad1\""), "{copied}");
    assert!(copied.contains("invoked_skills"), "{copied}");
}

#[test]
fn a_portable_copy_refuses_a_bodyless_turn_record_instead_of_dropping_it() {
    // Portable replay re-emits every record through the reader, which takes nothing out of a
    // bodyless turn: refuse before publishing rather than mint a session missing a record.
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = session_attachment_and_bad_tail(&dir);
    let before = std::fs::read(&src).unwrap();
    let out = dir.join("out");
    let mut o = opts(CutAt::End, &out);
    o.thinking = Some(crate::emit::ThinkingMode::Portable);
    let err = rewind_session(&sref(&src, SID), &o).unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("line 4"), "{msg}");
    assert!(msg.contains("`user`") && msg.contains("no `message` body"), "{msg}");
    assert!(msg.contains("bad1"), "the refusal names the record: {msg}");
    assert!(msg.contains("without re-emitting"), "{msg}");
    // Nothing was published, and no staged copy was left behind.
    assert!(!out.join("22222222-2222-4222-8222-222222222222.jsonl").exists());
    assert!(!out.join("22222222-2222-4222-8222-222222222222.rewind.json").exists());
    assert!(std::fs::read_dir(&out)
        .unwrap()
        .flatten()
        .all(|e| !e.file_name().to_string_lossy().contains("rewind")));
    assert_eq!(std::fs::read(&src).unwrap(), before, "the source is untouched");
}

#[test]
fn without_the_unreadable_record_the_attachment_rides_along() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = session_attachment_tail(&dir);
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::End, &dir.join("out"))).unwrap();

    let got = read_lines(&res.new_path);
    assert_eq!(
        got.iter().map(ty).collect::<Vec<_>>(),
        ["user", "assistant", "attachment"]
    );
    assert_eq!(got[2]["attachment"]["skills"][0]["name"], "fresh");
    assert_eq!(got[2]["rendered"], "padding");
    assert_eq!(got[2]["sessionId"], "22222222-2222-4222-8222-222222222222");
    assert_eq!(res.omitted_lines, 0);
}

#[test]
fn end_refuses_invalid_json_tail_in_native_and_staged_copies() {
    for thinking in [crate::emit::ThinkingMode::Native, crate::emit::ThinkingMode::Portable] {
        for tail in ["{\"type\":\"cost-state\"", "not-json\n", "{\"type\":}\n", "\u{000c}\n"] {
            let world = crate::test_world("rewind");
            let dir = world.proj.clone();
            let src = session_tailed(&dir);
            std::fs::OpenOptions::new()
                .append(true)
                .open(&src)
                .unwrap()
                .write_all(tail.as_bytes())
                .unwrap();
            let before = std::fs::read(&src).unwrap();
            let out = dir.join("out");
            let options = RewindOptions {
                thinking: Some(thinking),
                ..opts(CutAt::End, &out)
            };
            let result = rewind_session(&sref(&src, SID), &options);
            assert!(result.is_err(), "invalid tail published under {thinking:?}: {tail:?}");
            assert!(std::fs::read_dir(&out).unwrap().next().is_none());
            assert_eq!(std::fs::read(&src).unwrap(), before);
            // A deliberate cut need not validate the bytes it omits.
            let cut = rewind_session(
                &sref(&src, SID),
                &RewindOptions {
                    at: CutAt::Message(1),
                    ..options
                },
            )
            .unwrap();
            assert_eq!(cut.lines_written, 2);
            assert_eq!(cut.omitted_lines, 4);
        }
    }
}

/// An explicit cut whose message leaves a call open scans on past the cut (`Closing`) for the
/// result. A malformed line on the way is a future record the cut never meant to copy: it is
/// omitted, and the result after it still closes the call. The same bytes under `End` sit inside
/// the copied window and refuse, so the omission is the cut's, not a relaxed parse.
#[test]
fn an_explicit_cut_omits_a_malformed_line_while_it_closes_an_open_call() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = dir.join(format!("{SID}.jsonl"));
    write_jsonl(
        &src,
        &[
            rec("user", "u0", None, text("user", "read it")),
            rec("assistant", "a0", Some("u0"), bash("t1", "cat notes")),
        ],
    );
    let mut tail = b"not-json\n".to_vec();
    for v in [
        rec("user", "t1r", Some("a0"), result("t1", "notes")),
        rec("assistant", "a1", Some("t1r"), text("assistant", "read")),
    ] {
        tail.extend(v.to_string().into_bytes());
        tail.push(b'\n');
    }
    append(&src, &tail);
    let cut = idx_of(&src, "a0");

    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Message(cut), &dir.join("out"))).unwrap();
    assert_eq!(uuids(&read_lines(&res.new_path)), ["u0", "a0", "t1r"]);
    assert_eq!((res.cut_line, res.end_line), (2, 4));
    assert_eq!((res.closed_tool_calls, res.open_tool_calls), (1, 0));
    assert_eq!(res.omitted_lines, 2, "the malformed line and the turn after the close");

    let err = rewind_session(&sref(&src, SID), &opts(CutAt::End, &dir.join("end"))).unwrap_err();
    assert!(
        err.to_string().contains("line 3 is not a complete JSON record"),
        "{err}"
    );
}

/// The same, with the call never closed: the scan reaches EOF, the open call is reported, and a
/// live session's half-written last line (no newline) is omitted rather than refused.
#[test]
fn an_explicit_cut_left_open_omits_a_half_written_last_line() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = dir.join(format!("{SID}.jsonl"));
    write_jsonl(
        &src,
        &[
            rec("user", "u0", None, text("user", "read it")),
            rec("assistant", "a0", Some("u0"), bash("t1", "cat notes")),
        ],
    );
    let cut = idx_of(&src, "a0");
    append(&src, b"{\"type\":\"user\"");
    let before = std::fs::read(&src).unwrap();

    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Message(cut), &dir.join("out"))).unwrap();
    assert_eq!(uuids(&read_lines(&res.new_path)), ["u0", "a0"]);
    assert_eq!((res.closed_tool_calls, res.open_tool_calls), (0, 1));
    assert_eq!(res.omitted_lines, 1);
    assert_eq!(std::fs::read(&src).unwrap(), before, "the source must never change");
}

/// Under `End` the last line of the captured prefix is copied, so a half-written one refuses; with
/// no newline the session may still be appending it, and the refusal says to rerun before it says
/// to repair. The same bytes WITH a newline are a finished bad record: no rerun is offered.
#[test]
fn end_names_a_rerun_for_an_unterminated_last_line_only() {
    for (tail, rerun) in [
        ("{\"type\":\"cost-state\"", true),
        ("{\"type\":\"cost-state\"\n", false),
    ] {
        let world = crate::test_world("rewind");
        let dir = world.proj.clone();
        let src = session_tailed(&dir);
        append(&src, tail.as_bytes());
        let out = dir.join("out");
        let msg = rewind_session(&sref(&src, SID), &opts(CutAt::End, &out))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("line 6"), "{msg}");
        assert_eq!(
            msg.contains("unterminated") && msg.contains("rerun the rewind"),
            rerun,
            "{msg}"
        );
        assert_eq!(msg.contains("not a complete JSON record"), !rerun, "{msg}");
        assert!(std::fs::read_dir(&out).unwrap().next().is_none(), "nothing published");
    }
}

#[test]
fn present_message_body_variants_remain_parser_compatible() {
    for body in [
        json!(null),
        json!(false),
        json!(7),
        json!("bad"),
        json!([]),
        json!({}),
        json!({"content": null}),
        json!({"content": 7}),
        json!({"content": {}}),
    ] {
        for ty in ["user", "assistant"] {
            let world = crate::test_world("rewind");
            let dir = world.proj.clone();
            let src = session_tailed(&dir);
            let bad = rec(ty, "invalid-body", Some("a0"), body.clone());
            std::fs::OpenOptions::new()
                .append(true)
                .open(&src)
                .unwrap()
                .write_all((bad.to_string() + "\n").as_bytes())
                .unwrap();
            let before = std::fs::read(&src).unwrap();
            let out = dir.join("out");
            let parsed = crate::harness::claude::parse_str("synthetic", std::str::from_utf8(&before).unwrap(), None);
            assert_eq!(parsed.messages.len(), 3, "present body produces a message: {body}");
            let result = rewind_session(&sref(&src, SID), &opts(CutAt::End, &out)).unwrap();
            assert_eq!(read_lines(&result.new_path)[5]["message"], body);
            assert_eq!((result.lines_written, result.omitted_lines), (6, 0));
            assert_eq!(std::fs::read(&src).unwrap(), before);
        }
    }
}

#[test]
fn valid_empty_content_and_unknown_tail_survive_all_copy_paths() {
    for thinking in [crate::emit::ThinkingMode::Native, crate::emit::ThinkingMode::Portable] {
        let world = crate::test_world("rewind");
        let dir = world.proj.clone();
        let src = session_attachment_tail(&dir);
        let extra = [
            rec("user", "empty", Some("a0"), json!({"role":"user", "content":[]})),
            json!({"type":"future-state", "sessionId":SID, "payload":{"untouched":true}}),
        ];
        for v in extra {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&src)
                .unwrap()
                .write_all((v.to_string() + "\n").as_bytes())
                .unwrap();
        }
        let options = RewindOptions {
            thinking: Some(thinking),
            ..opts(CutAt::End, &dir.join("out"))
        };
        let res = rewind_session(&sref(&src, SID), &options).unwrap();
        let got = read_lines(&res.new_path);
        assert_eq!(got.len(), 5);
        assert_eq!(got[4]["payload"]["untouched"], true);
        assert_eq!(got[4]["sessionId"], res.new_id);
        assert_eq!((res.end_line, res.omitted_lines), (5, 0));
        assert_eq!(got[2]["attachment"]["skills"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn full_changes_start_but_end_still_includes_compacted_tail() {
    for full in [false, true] {
        let world = crate::test_world("rewind");
        let dir = world.proj.clone();
        let src = session(&dir);
        for tail in tail_records() {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&src)
                .unwrap()
                .write_all((tail.to_string() + "\n").as_bytes())
                .unwrap();
        }
        let res = rewind_session(
            &sref(&src, SID),
            &RewindOptions {
                full,
                ..opts(CutAt::End, &dir.join("out"))
            },
        )
        .unwrap();
        assert_eq!(
            (res.start_line, res.end_line, res.omitted_lines),
            (if full { 1 } else { 8 }, 20, 0)
        );
        assert_eq!(read_lines(&res.new_path).last().unwrap()["type"], "cost-state");
    }
}

#[test]
fn empty_and_metadata_only_sources_refuse_without_publication() {
    for body in ["", "\n", "{\"type\":\"cost-state\",\"sessionId\":\"synthetic\"}\n"] {
        let world = crate::test_world("rewind");
        let dir = world.proj.clone();
        let src = dir.join(format!("{SID}.jsonl"));
        std::fs::write(&src, body).unwrap();
        let out = dir.join("out");
        let err = rewind_session(&sref(&src, SID), &opts(CutAt::End, &out)).unwrap_err();
        assert!(err.to_string().contains("no messages"), "{err}");
        assert!(!out.exists());
        assert_eq!(std::fs::read_to_string(src).unwrap(), body);
    }
}

#[test]
fn a_cut_between_parallel_results_runs_on_until_the_calls_close() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = session(&dir);
    let res = rewind_session(
        &sref(&src, SID),
        &opts(CutAt::Message(idx_of(&src, "t1r")), &dir.join("o")),
    )
    .unwrap();
    // t2 was issued before the cut; its result (line 15, after a hook record) comes along, the
    // next assistant turn does not.
    assert_eq!((res.cut_line, res.end_line), (13, 15));
    assert_eq!((res.closed_tool_calls, res.open_tool_calls), (1, 0));
    let got = read_lines(&res.new_path);
    assert_eq!(uuids(&got).last(), Some(&"t2r"));
    assert!(res.warnings.is_empty(), "{:?}", res.warnings);
}

#[test]
fn provenance_sidecar_records_source_cut_and_start_and_the_source_is_untouched() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = session(&dir);
    let before = std::fs::read(&src).unwrap();
    let out = dir.join("out");
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::Commit(SHA[..12].into()), &out)).unwrap();
    assert_eq!(std::fs::read(&src).unwrap(), before, "the source must never change");

    assert_eq!(
        res.provenance_path,
        out.join("22222222-2222-4222-8222-222222222222.rewind.json")
    );
    let p: Value = serde_json::from_str(&std::fs::read_to_string(&res.provenance_path).unwrap()).unwrap();
    assert_eq!(p["format"], "cv-rewind");
    assert_eq!(p["source"]["id"], SID);
    assert_eq!(p["source"]["path"], src.display().to_string());
    assert_eq!(p["source"]["bytes"], before.len() as u64);
    assert_eq!(p["source"]["sha256"], crate::digest::sha256_hex(&before));
    assert_eq!(p["source"]["subagent"], false);
    assert_eq!(p["cut"]["by"], "commit");
    assert_eq!(p["cut"]["msg_idx"], idx_of(&src, "r1"));
    assert_eq!(p["cut"]["line"], 7);
    assert_eq!(p["cut"]["evidence"]["kind"], "created");
    assert_eq!(p["start"]["mode"], "from_compaction");
    assert_eq!(p["start"]["msg_idx"], idx_of(&src, "b1"));
    assert_eq!(p["start"]["line"], 4);
    assert_eq!(p["omitted_tail"]["lines"], 10);
    assert!(p["omitted_tail"]["note"].as_str().unwrap().contains("10 more line(s)"));
    assert_eq!(p["cv_version"], "cv-test");
    assert!(p["derived_at"].as_str().unwrap().starts_with("20"));

    // A dry run computes the same window and writes nothing.
    let dry = dir.join("dry");
    let d = rewind_session(
        &sref(&src, SID),
        &RewindOptions {
            dry_run: true,
            ..opts(CutAt::Commit(SHA.into()), &dry)
        },
    )
    .unwrap();
    assert_eq!((d.start_line, d.cut_line), (4, 7));
    assert!(!dry.exists());
    // An existing target is refused, never overwritten.
    let err = rewind_session(&sref(&src, SID), &opts(CutAt::End, &out)).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[test]
fn subagent_transcript_becomes_a_standalone_resumable_session() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let parent = dir.join(format!("{SID}.jsonl"));
    write_jsonl(
        &parent,
        &[rec("user", "p0", None, json!({"role": "user", "content": "spawn"}))],
    );
    let agent = dir.join(SID).join("subagents").join("agent-abc123.jsonl");
    let side = |ty: &str, uuid: &str, parent: Option<&str>, message: Value| {
        let mut v = rec(ty, uuid, parent, message);
        v["isSidechain"] = json!(true);
        v["agentId"] = json!("abc123");
        v["cwd"] = json!("/work/proj/wt"); // the sub-agent works in a worktree…
        v
    };
    write_jsonl(
        &agent,
        &[
            side("user", "x0", None, json!({"role": "user", "content": "fix the gate"})),
            side(
                "assistant",
                "x1",
                Some("x0"),
                bash("toolu_x", "git commit -qam fix && git rev-parse HEAD"),
            ),
            side("user", "x2", Some("x1"), result("toolu_x", SHA)),
            side("assistant", "x3", Some("x2"), text("assistant", "committed")),
        ],
    );
    let before = std::fs::read(&agent).unwrap();
    let r = sref(&agent, "agent-abc123");
    assert!(is_subagent(&r));
    // Default out = the PARENT's project dir (where `claude --resume` looks), not `subagents/`.
    assert_eq!(project_dir(&agent), dir);
    let res = rewind_session(
        &r,
        &RewindOptions {
            out_dir: None,
            ..opts(CutAt::End, &dir)
        },
    )
    .unwrap();
    assert!(res.subagent);
    assert_eq!(res.new_path.parent().unwrap(), dir);
    assert_eq!(res.source_session_id.as_deref(), Some(SID));
    let got = read_lines(&res.new_path);
    assert!(got
        .iter()
        .all(|v| v["isSidechain"] == false && v.get("agentId").is_none()));
    assert!(got
        .iter()
        .all(|v| v["sessionId"] == "22222222-2222-4222-8222-222222222222"));
    // The whole transcript is one root chain the loader can walk to a null parent.
    assert_eq!(chain(&got), ["x3", "x2", "x1", "x0"]);
    // …but resumes from the parent's launch dir, the one Claude files `-work-proj` under.
    assert_eq!(res.cwd.as_deref(), Some("/work/proj"));
    assert_eq!(res.cut_cwd.as_deref(), Some("/work/proj/wt"));
    assert!(res.warnings.is_empty(), "{:?}", res.warnings);
    // And it reads back as an ordinary (non-sub-agent) session.
    let s = crate::harness::claude::parse_str("n", &std::fs::read_to_string(&res.new_path).unwrap(), None);
    assert_eq!(s.messages.len(), 4);
    assert_eq!(std::fs::read(&agent).unwrap(), before);

    // A sub-agent's commit is exact evidence too (a full `rev-parse` sha names a short query).
    let short = rewind_session(
        &r,
        &RewindOptions {
            dry_run: true,
            ..opts(CutAt::Commit(SHA[..7].into()), &dir)
        },
    )
    .unwrap();
    assert_eq!(short.cut_line, 3);
}

#[test]
fn commit_evidence_reports_only_git_proof_of_the_queried_shas() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = session(&dir);
    let r = sref(&src, SID);
    let ev = commit_evidence(&r, &[SHA.to_string(), "0123456789abcdef".into()]).unwrap();
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!((ev[0].kind, ev[0].msg_idx), (EvidenceKind::Created, idx_of(&src, "r1")));
    assert!(ev[0].byte_offset.is_none(), "plain lazy() streams carry no offsets");
    assert!(commit_evidence(&r, &["fedcba9876".into()]).unwrap().is_empty());
    assert!(commit_evidence(&r, &["not-a-sha".into()]).unwrap().is_empty());
}

#[test]
fn file_mentions_finds_needles_across_chunk_boundaries() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let p = dir.join("big.jsonl");
    // Straddle the 4 MiB read boundary with one needle; put the other at the very end.
    let mut body = vec![b'x'; (4 << 20) - 3];
    body.extend_from_slice(b"ABCDEF1");
    body.extend_from_slice(&vec![b'y'; 1000]);
    body.extend_from_slice(b"9876543");
    std::fs::write(&p, &body).unwrap();
    assert_eq!(
        file_mentions(&p, &["abcdef1", "9876543", "1111111"]).unwrap(),
        [true, true, false]
    );
}

#[test]
fn sha_tokens_match_whole_hex_runs_prefix_compatibly() {
    let sha = normalize_sha(SHA).unwrap();
    assert!(output_names_sha("[main 63cef47] gate fix", &sha));
    assert!(output_names_sha("   1a2b3c4..63cef473b77  HEAD -> main", &sha));
    assert!(output_names_sha(SHA, &normalize_sha("63CEF47").unwrap()));
    // Inside a longer hex run that merely CONTAINS the sha's digits: not a match.
    assert!(!output_names_sha("ff63cef473b771", &sha));
    // Too short to mean anything.
    assert!(!output_names_sha("[main 63cef4] x", &sha));
    assert!(!output_names_sha("[main 63cef48] x", &sha));
    assert_eq!(normalize_sha(" 63CEF47 "), Some("63cef47".into()));
    assert_eq!(normalize_sha("63cef4"), None);
    assert_eq!(normalize_sha("63cef4z9"), None);
}

#[test]
fn git_verbs_classify_created_and_pushed() {
    use EvidenceKind::*;
    assert_eq!(git_evidence_kind("git commit -m x"), Some(Created));
    assert_eq!(
        git_evidence_kind("cd /r && git -C /r -c user.name=a commit -qm 'x'"),
        Some(Created)
    );
    assert_eq!(git_evidence_kind("git --no-pager merge feature"), Some(Created));
    assert_eq!(git_evidence_kind("git cherry-pick abc1234"), Some(Created));
    assert_eq!(git_evidence_kind("git push -u origin HEAD"), Some(Pushed));
    // A push after a commit in one command is still a creation.
    assert_eq!(git_evidence_kind("git push; git commit -am y"), Some(Created));
    assert_eq!(git_evidence_kind("git log --oneline"), None);
    assert_eq!(git_evidence_kind("git commit-tree abc"), None);
    assert_eq!(git_evidence_kind("echo 'legit commit'"), None);
    assert_eq!(git_evidence_kind("/usr/bin/git commit -m z"), Some(Created));
}

#[test]
fn rewind_prefix_receipt_accepts_appends_and_rejects_source_mutations() {
    for mutation in ["append", "rewrite", "replace", "rewrite-grow", "truncate"] {
        let world = crate::test_world("rewind");
        let dir = world.proj.clone();
        let path = dir.join(format!("{SID}.jsonl"));
        let initial = rec("user", "u", None, text("user", "original prefix"));
        write_jsonl(&path, &[initial]);
        let original = std::fs::read(&path).unwrap();
        let r = SessionRef::from_path(&path, Harness::Claude);
        let opts = RewindOptions {
            new_id: Some("derived".into()),
            out_dir: Some(dir.clone()),
            ..opts(CutAt::End, &dir)
        };
        let result = rewind_session_after_scan(&r, &opts, || {
            match mutation {
                "append" => {
                    let mut writer = std::fs::OpenOptions::new().append(true).open(&path)?;
                    writer.write_all(b"\n")?;
                }
                "replace" => {
                    let replacement = dir.join("replacement.jsonl");
                    std::fs::write(&replacement, &original)?;
                    std::fs::rename(replacement, &path)?;
                }
                "truncate" => {
                    std::fs::write(&path, b"")?;
                }
                other => {
                    let mut changed = original.clone();
                    let start = changed
                        .windows(b"original".len())
                        .position(|w| w == b"original")
                        .unwrap();
                    changed[start..start + 8].copy_from_slice(b"modified");
                    if other == "rewrite-grow" {
                        changed.extend_from_slice(b"\n");
                    }
                    std::fs::write(&path, changed)?;
                }
            }
            Ok(())
        });
        if mutation == "append" {
            let result = result.unwrap();
            assert_eq!(result.source_bytes, original.len() as u64);
            assert_eq!(result.source_sha256, {
                let mut hash = crate::digest::Sha256::new();
                hash.update(&original);
                hash.finish_hex()
            });
            let actual = std::fs::read_to_string(result.new_path).unwrap();
            assert_eq!(
                actual,
                crate::restamp::restamp(std::str::from_utf8(&original).unwrap(), "derived").body
            );
        } else {
            assert!(result.is_err(), "{mutation} must not derive inconsistent output");
            assert!(!dir.join("derived.jsonl").exists());
            assert!(!dir.join("derived.rewind.json").exists());
        }
        assert!(std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().starts_with(".cv-rewind-")));
    }
}

#[test]
fn rewind_rejects_output_ids_that_escape_the_output_directory() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let source = dir.join(format!("{SID}.jsonl"));
    write_jsonl(&source, &[rec("user", "u", None, text("user", "original"))]);
    let reference = SessionRef::from_path(&source, Harness::Claude);
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
        let error = rewind_session(
            &reference,
            &RewindOptions {
                new_id: Some(id.into()),
                ..opts(CutAt::End, &dir)
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("filename component"), "{error}");
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
}

#[test]
fn checked_derivation_preserves_large_results_and_closes_parallel_calls() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let path = dir.join(format!("{SID}.jsonl"));
    let payload = "λ payload\n".repeat(200000);
    write_jsonl(
        &path,
        &[
            rec("user", "u", None, text("user", "read both")),
            rec(
                "assistant",
                "a",
                Some("u"),
                json!({"role":"assistant","content":[
            {"type":"tool_use","id":"toolu_a","name":"Read","input":{"file_path":"a"}},
            {"type":"tool_use","id":"toolu_b","name":"Read","input":{"file_path":"b"}}]}),
            ),
            rec("user", "r1", Some("a"), result("toolu_a", &payload)),
            rec("user", "r2", Some("r1"), result("toolu_b", "second")),
            rec("assistant", "future", Some("r2"), text("assistant", "future")),
        ],
    );
    let original = std::fs::read_to_string(&path).unwrap();
    let result = rewind_session(
        &sref(&path, SID),
        &RewindOptions {
            full: true,
            ..opts(CutAt::Message(1), &dir.join("out"))
        },
    )
    .unwrap();
    assert_eq!(
        (result.closed_tool_calls, result.open_tool_calls, result.lines_written),
        (2, 0, 4)
    );
    let expected: String = original
        .lines()
        .take(4)
        .map(|line| crate::restamp_line(line, &result.new_id).unwrap() + "\n")
        .collect();
    assert_eq!(std::fs::read_to_string(&result.new_path).unwrap(), expected);
    crate::harness::claude::emit::verify_claude_resumable(&result.new_path, None).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
}

/// A cut whose call is answered only after the user's next prompt: the copy stops at the prompt,
/// so the rewind would hold a call its source answers. It is refused before anything is published.
#[test]
fn a_cut_that_would_leave_an_answered_call_open_is_refused_and_leaves_nothing() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = dir.join(format!("{SID}.jsonl"));
    write_jsonl(
        &src,
        &[
            rec("user", "u0", None, text("user", "read it")),
            rec("assistant", "a0", Some("u0"), bash("t1", "cat notes")),
            rec("user", "u1", Some("a0"), text("user", "never mind")),
            rec("assistant", "a1", Some("u1"), text("assistant", "stopping")),
            rec("user", "t1r", Some("a1"), result("t1", "notes")),
        ],
    );
    let out = dir.join("out");
    let err = rewind_session(&sref(&src, SID), &opts(CutAt::Message(idx_of(&src, "a0")), &out))
        .unwrap_err()
        .to_string();
    assert!(err.contains("tool_use t1 has no tool_result"), "{err}");
    assert!(err.contains("nothing was written"), "{err}");
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
    // A call its source never answers either (a live session's last) is kept.
    let live = dir.join("live");
    write_jsonl(
        &live.join(format!("{SID}.jsonl")),
        &[
            rec("user", "u0", None, text("user", "read it")),
            rec("assistant", "a0", Some("u0"), bash("t1", "cat notes")),
        ],
    );
    let src = live.join(format!("{SID}.jsonl"));
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::End, &live.join("out"))).unwrap();
    assert_eq!(uuids(&read_lines(&res.new_path)), ["u0", "a0"]);
}

/// Claude Code writes each parallel `tool_use` of one response as its own record: a cut at the
/// first keeps its siblings and runs on to both results, so no answered call is left open.
#[test]
fn a_cut_at_one_of_a_responses_parallel_calls_keeps_its_siblings() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = dir.join(format!("{SID}.jsonl"));
    let call = |id: &str| {
        let mut m = bash(id, "cat notes");
        m["id"] = json!("msg_1");
        m
    };
    write_jsonl(
        &src,
        &[
            rec("user", "u0", None, text("user", "read both")),
            rec("assistant", "a0", Some("u0"), call("t1")),
            rec("assistant", "a1", Some("a0"), call("t2")),
            rec("user", "t1r", Some("a0"), result("t1", "one")),
            rec("user", "t2r", Some("a1"), result("t2", "two")),
            rec("assistant", "a2", Some("t2r"), text("assistant", "both read")),
        ],
    );
    let res = rewind_session(
        &sref(&src, SID),
        &opts(CutAt::Message(idx_of(&src, "a0")), &dir.join("out")),
    )
    .unwrap();
    assert_eq!(uuids(&read_lines(&res.new_path)), ["u0", "a0", "a1", "t1r", "t2r"]);
    assert_eq!((res.closed_tool_calls, res.open_tool_calls), (2, 0));
    assert!(res.warnings.is_empty(), "{:?}", res.warnings);
}

/// Past the cut only a result for an open call is copied: a result written before its own call
/// (Claude Code records them out of order) would leave the copy's leaf pointing at a call the
/// copy does not hold.
#[test]
fn a_result_for_no_open_call_ends_the_copy() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = dir.join(format!("{SID}.jsonl"));
    write_jsonl(
        &src,
        &[
            rec("user", "u0", None, text("user", "read it")),
            rec(
                "assistant",
                "a0",
                Some("u0"),
                json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {}},
                    {"type": "tool_use", "id": "t3", "name": "Bash", "input": {}}]}),
            ),
            rec("user", "t1r", Some("a0"), result("t1", "notes")),
            rec("user", "t2r", Some("a1"), result("t2", "early")),
            rec("assistant", "a1", Some("t1r"), bash("t2", "cat more")),
        ],
    );
    let res = rewind_session(
        &sref(&src, SID),
        &opts(CutAt::Message(idx_of(&src, "a0")), &dir.join("out")),
    )
    .unwrap();
    assert_eq!(uuids(&read_lines(&res.new_path)), ["u0", "a0", "t1r"]);
    // `t3` is never answered, in the source either.
    assert_eq!((res.closed_tool_calls, res.open_tool_calls), (1, 1));
}

/// An empty line is blank: copied verbatim, never an error.
#[test]
fn an_empty_line_is_copied_like_any_blank_one() {
    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = dir.join(format!("{SID}.jsonl"));
    write_jsonl(
        &src,
        &[
            rec("user", "u0", None, text("user", "go")),
            rec("assistant", "a0", Some("u0"), text("assistant", "done")),
        ],
    );
    let body = std::fs::read_to_string(&src).unwrap().replacen('\n', "\n\n", 1);
    std::fs::write(&src, body).unwrap();
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::End, &dir.join("out"))).unwrap();
    assert_eq!(std::fs::read_to_string(res.new_path).unwrap().lines().nth(1), Some(""));
}

// ---------------------------------------------------------------------------
// Request snapshots (`api-request-blob`) are physical evidence, never events
// ---------------------------------------------------------------------------

/// A top-level `api-request-blob` record embedding a verbatim message, the shape a request
/// snapshot carries: the same `message.content` blocks a turn has, under a type no turn has.
fn blob(uuid: &str, message: Value) -> Value {
    json!({"type": "api-request-blob", "sessionId": SID, "uuid": uuid,
           "parentUuid": null, "isSidechain": false, "message": message})
}

fn sidechain_rec(ty: &str, uuid: &str, parent: Option<&str>, message: Value) -> Value {
    let mut v = rec(ty, uuid, parent, message);
    v["isSidechain"] = json!(true);
    v
}

/// Read `v` the way the rewind pass does (a captured source, so `Facts` has its byte ranges),
/// then feed it to the tracker.
fn track(v: &Value, open: &mut HashMap<String, Option<String>>, closed: Option<&mut usize>) {
    let world = crate::test_world("rewind");
    let path = world.proj.join("record.jsonl");
    std::fs::write(&path, v.to_string()).unwrap();
    let source = crate::json::JsonSource::capture(&path).unwrap();
    let facts = source
        .read(
            crate::json::ReadInput::Range(0, source.len()),
            crate::json::Validation::Value,
            |view| Ok(Facts::parse(view)?.0),
        )
        .unwrap();
    track_tool_calls(&facts, open, closed);
}

fn ids(open: &HashMap<String, Option<String>>) -> Vec<&str> {
    let mut ids: Vec<&str> = open.keys().map(String::as_str).collect();
    ids.sort_unstable();
    ids
}

#[test]
fn a_snapshot_replaying_a_closed_use_reopens_nothing() {
    let mut open = HashMap::new();
    track(&rec("assistant", "a1", None, bash("toolu_x", "ls")), &mut open, None);
    track(&rec("user", "r1", Some("a1"), result("toolu_x", "ok")), &mut open, None);
    track(&blob("snap1", bash("toolu_x", "ls")), &mut open, None);
    assert!(open.is_empty(), "{open:?}");

    let world = crate::test_world("rewind");
    let dir = world.proj.clone();
    let src = dir.join(format!("{SID}.jsonl"));
    write_jsonl(
        &src,
        &[
            rec("user", "u0", None, text("user", "go")),
            rec("assistant", "a1", Some("u0"), bash("toolu_x", "ls")),
            rec("user", "r1", Some("a1"), result("toolu_x", "ok")),
            blob("snap1", bash("toolu_x", "ls")),
            rec("assistant", "a2", Some("r1"), text("assistant", "done")),
        ],
    );
    let res = rewind_session(&sref(&src, SID), &opts(CutAt::End, &dir.join("out"))).unwrap();
    assert_eq!((res.closed_tool_calls, res.open_tool_calls), (0, 0));
    assert!(res.warnings.is_empty(), "{:?}", res.warnings);
}

#[test]
fn a_real_assistant_use_after_the_pair_opens_one() {
    let mut open = HashMap::new();
    track(&rec("assistant", "a1", None, bash("toolu_x", "ls")), &mut open, None);
    track(&rec("user", "r1", Some("a1"), result("toolu_x", "ok")), &mut open, None);
    track(
        &rec("assistant", "a2", Some("r1"), bash("toolu_y", "pwd")),
        &mut open,
        None,
    );
    assert_eq!(ids(&open), ["toolu_y"]);
}

#[test]
fn a_snapshot_result_neither_closes_nor_counts() {
    let mut open = HashMap::new();
    let mut closed = 0usize;
    track(&rec("assistant", "a1", None, bash("toolu_x", "ls")), &mut open, None);
    track(&blob("snap1", result("toolu_x", "ok")), &mut open, Some(&mut closed));
    assert_eq!(ids(&open), ["toolu_x"]);
    assert_eq!(closed, 0);
    track(
        &rec("user", "r1", Some("a1"), result("toolu_x", "ok")),
        &mut open,
        Some(&mut closed),
    );
    assert!(open.is_empty(), "{open:?}");
    assert_eq!(closed, 1);
}

#[test]
fn a_same_id_snapshot_replay_stays_closed_but_a_real_reissue_reopens() {
    let mut open = HashMap::new();
    track(&rec("assistant", "a1", None, bash("toolu_a", "ls")), &mut open, None);
    track(&rec("user", "r1", Some("a1"), result("toolu_a", "ok")), &mut open, None);
    track(&blob("snap1", bash("toolu_a", "ls")), &mut open, None);
    assert!(open.is_empty(), "{open:?}");
    track(
        &rec("assistant", "a2", Some("r1"), bash("toolu_a", "ls")),
        &mut open,
        None,
    );
    assert_eq!(ids(&open), ["toolu_a"]);
}

#[test]
fn sidechain_records_still_count_as_conversation() {
    let mut open = HashMap::new();
    track(
        &sidechain_rec("assistant", "a1", None, bash("toolu_s", "ls")),
        &mut open,
        None,
    );
    assert_eq!(ids(&open), ["toolu_s"]);
    track(
        &sidechain_rec("user", "r1", Some("a1"), result("toolu_s", "ok")),
        &mut open,
        None,
    );
    assert!(open.is_empty(), "{open:?}");
}
