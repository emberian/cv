//! Roo Code adapter — a Cline fork with the **same** per-task file format.
//!
//! Roo stores tasks under a different VS Code extension namespace
//! (`rooveterinaryinc.roo-cline` and the newer `rooveterinaryinc.roo-code`) plus a plain `~/.roo/`,
//! and the Roo CLI writes the same layout under `~/.vscode-mock/global-storage/tasks/`.
//! The on-disk layout inside each `tasks/<taskId>/` is identical to Cline's
//! (`api_conversation_history.json` array of Anthropic messages + `ui_messages.json` +
//! `task_metadata.json`), so all real *parse* logic lives in [`super::cline`] as `pub` helpers and we
//! just point them at Roo's roots and tag the output `Harness::Roo`.
//!
//! # Emit (conversion target)
//!
//! [`emit`] is [`super::cline::emit_with_harness`] with `Harness::Roo`: the per-task layout is the
//! same bytes for both harnesses, so Roo owns no writer of its own — only the resume hint names the
//! editor extension to open the task in.

use super::cline::{discover_in, emit_with_harness, stream_task_dir, task_dir_dependencies, task_roots, ROO_EXT_IDS};
use super::Adapter;
use crate::ir::*;
use crate::source::{Dependencies, Probe};
use crate::stream::{Demand, MessageSink};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct Roo {
    roots: Vec<PathBuf>,
}

/// This adapter with no roots: it discovers nothing and answers every fact that does not depend on
/// where the store lives ([`super::rootless`]).
pub(crate) static ROOTLESS: Roo = Roo { roots: Vec::new() };

impl Roo {
    pub fn detect() -> Self {
        let mut roots = task_roots(ROO_EXT_IDS, ".roo");
        roots.extend(crate::env::home_dir().and_then(|h| cli_tasks_root(&h)));
        Roo { roots }
    }

    /// Roo's task stores under an explicit home: the Roo CLI's mock-VS-Code globalStorage and a
    /// plain `.roo/tasks` (the editor globalStorage dirs are platform paths, not home-relative).
    pub fn with_home(home: &Path) -> Self {
        let mut roots: Vec<PathBuf> = cli_tasks_root(home).into_iter().collect();
        let plain = home.join(".roo").join("tasks");
        if plain.is_dir() {
            roots.push(plain);
        }
        Roo { roots }
    }
}

/// The Roo Code CLI (GitHub `cli-v*` releases) runs the extension in a mock VS Code host whose
/// globalStorage is `~/.vscode-mock/global-storage`, so its tasks land in `tasks/` there with the
/// extension's own layout.
fn cli_tasks_root(home: &Path) -> Option<PathBuf> {
    let p = home.join(".vscode-mock").join("global-storage").join("tasks");
    p.is_dir().then_some(p)
}

impl Adapter for Roo {
    fn harness(&self) -> Harness {
        Harness::Roo
    }

    fn dependencies(&self, r: &SessionRef, _probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        task_dir_dependencies(r, deps);
        Some(())
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn discover(&self) -> Result<crate::Discovered> {
        Ok(discover_in(&self.roots, Harness::Roo))
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        crate::stream::checked(sink, |sink| {
            // Spawn turns, sub-agent returns and deferred tool calls, typed (`crate::affordance`).
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Roo, sink);
            let sink: &mut dyn MessageSink = &mut spawns;
            stream_task_dir(&r.path, &r.id, Harness::Roo, opts, sink)
        })
    }

    /// The checkpoints and permission asks `ui_messages.json` logs (the conversation file has
    /// neither): `say: checkpoint_saved` (a shadow-git commit under `checkpoints/`) and the `ask`s
    /// that gate a tool, a command, an MCP call or a browser launch.
    fn store_affordances(&self, r: &SessionRef) -> anyhow::Result<Vec<crate::affordance::Affordance>> {
        let path = r.path.join("ui_messages.json");
        let read = UI.with(|slot| slot.borrow_mut().take());
        match read {
            Some((at, state, affordances)) if at == path && crate::source::FileId::at(&path) == state => {
                Ok(affordances)
            }
            _ => ui_affordances(&path),
        }
    }
}

thread_local! {
    /// The affordances [`ui_times`] read from one `ui_messages.json` at one state, for the
    /// [`Roo::store_affordances`] that follows its read on this thread: one file, taken once.
    static UI: std::cell::RefCell<UiRead> = const { std::cell::RefCell::new(None) };
}
type UiRead = Option<(
    PathBuf,
    Option<crate::source::FileId>,
    Vec<crate::affordance::Affordance>,
)>;

/// A task's `ui_messages.json` read once for its timestamp hints and its affordances, which wait
/// in [`UI`] for the count's [`Roo::store_affordances`]. `None` when the file does not decode or
/// cannot be read as a whole value: the caller reads it as before, and so does the affordance pass.
pub(super) fn ui_times(dir: &Path) -> Option<super::cline::UiTimes> {
    use serde_json::Value;
    let path = dir.join("ui_messages.json");
    #[cfg(test)]
    UI_READS.lock().unwrap().push(path.clone());
    let state = crate::source::FileId::at(&path);
    let value = crate::affordance::side_json(&path).ok()?;
    let mut times: super::cline::UiTimes = Default::default();
    if let Some(Value::Array(events)) = &value {
        let (mut first, mut last) = (None, None);
        for e in events {
            let ts = e
                .get("ts")
                .and_then(Value::as_i64)
                .and_then(chrono::DateTime::from_timestamp_millis);
            if let Some(dt) = ts {
                first.get_or_insert(dt);
                last = Some(dt);
            }
        }
        times = (first, last, first);
    }
    let affordances = match &value {
        Some(Value::Array(events)) => ui_affordances_of(events),
        _ => Vec::new(),
    };
    if crate::source::FileId::at(&path) == state {
        UI.with(|slot| *slot.borrow_mut() = Some((path, state, affordances)));
    }
    Some(times)
}

#[cfg(test)]
static UI_READS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn ui_affordances(path: &Path) -> Result<Vec<crate::affordance::Affordance>> {
    use serde_json::Value;
    #[cfg(test)]
    UI_READS.lock().unwrap().push(path.to_path_buf());
    let Some(Value::Array(events)) = crate::affordance::side_json(path)? else {
        return Ok(Vec::new());
    };
    Ok(ui_affordances_of(&events))
}

fn ui_affordances_of(events: &[serde_json::Value]) -> Vec<crate::affordance::Affordance> {
    use crate::affordance::{Affordance, Kind};
    use serde_json::Value;
    let mut out = Vec::new();
    for e in events {
        if e.get("partial").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let ts = e
            .get("ts")
            .and_then(Value::as_i64)
            .and_then(chrono::DateTime::from_timestamp_millis);
        let text = e.get("text").and_then(Value::as_str).unwrap_or("");
        // An ask's `text` is JSON for a tool or MCP call, the bare command line for a command.
        let json: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let field = |k: &str| json.get(k).and_then(Value::as_str).unwrap_or("");
        match (
            e.get("type").and_then(Value::as_str),
            e.get("ask").or(e.get("say")).and_then(Value::as_str),
        ) {
            (Some("say"), Some("checkpoint_saved")) => {
                let commit = text.get(..12).unwrap_or(text);
                out.push(Affordance::store_event(
                    Kind::Checkpoint,
                    "create",
                    ts,
                    "checkpoint_saved",
                    commit,
                ));
            }
            (Some("ask"), Some(what @ ("tool" | "command" | "use_mcp_server" | "browser_action_launch"))) => {
                let summary = match what {
                    "tool" => format!("{} {}", field("tool"), field("path")),
                    "use_mcp_server" => format!("{}.{}", field("serverName"), field("toolName")),
                    _ => text.to_string(),
                };
                out.push(Affordance::store_event(Kind::Approval, "ask", ts, what, summary.trim()));
            }
            _ => {}
        }
    }
    out
}

/// Emit `session` into Roo's per-task layout under `out_dir`: the Cline writer, tagged for Roo.
/// Registered in `emit::emitter_for`.
pub fn emit(session: &Session, out_dir: &Path, opts: &crate::emit::EmitOptions) -> Result<crate::harness::EmitResult> {
    emit_with_harness(session, out_dir, opts, Harness::Roo)
}

pub(crate) fn emit_staged(
    session: &Session,
    out_dir: &Path,
    opts: &crate::emit::EmitOptions,
    verify: Option<crate::emit::stage::VerifyFn<'_>>,
) -> Result<crate::harness::EmitResult> {
    super::cline::emit_with_harness_staged(session, out_dir, opts, Harness::Roo, verify)
}

#[cfg(test)]
mod tests {
    use super::super::cline::{parse_history_str, parse_task_dir};
    use super::*;
    use std::fs;

    fn fixture(name: &str) -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/roo/");
        std::fs::read_to_string(format!("{path}{name}")).unwrap_or_else(|e| panic!("reading fixture {name}: {e}"))
    }

    /// A count reads a task's `ui_messages.json` once: the stream's timestamp hints and the
    /// affordance pass share the read, and give what each read alone gave.
    #[test]
    fn a_count_reads_ui_messages_once() {
        let world = crate::test_world("roo-ui-once");
        let dir = world.base.join("1717111111111");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("api_conversation_history.json"),
            fixture("api_conversation_history.json"),
        )
        .unwrap();
        let ui = dir.join("ui_messages.json");
        fs::write(
            &ui,
            r#"[{"ts":1717111111000,"type":"say","say":"text","text":"hi"},
                {"ts":1717111112000,"type":"say","say":"checkpoint_saved","text":"0123456789abcdef"},
                {"ts":1717111113000,"type":"ask","ask":"command","text":"cargo test"}]"#,
        )
        .unwrap();
        let r = SessionRef::new("1717111111111", Harness::Roo, &dir);
        let reads = || {
            let mut all = UI_READS.lock().unwrap();
            let mine = all.iter().filter(|p| **p == ui).count();
            all.retain(|p| *p != ui);
            mine
        };
        // A counted read: the stream, then the affordance pass.
        let s = crate::stream::collect_with(&ROOTLESS, &r, &Demand::RICH).unwrap();
        assert_eq!(reads(), 1);
        assert_eq!(s.created_at, chrono::DateTime::from_timestamp_millis(1717111111000));
        // The same pass by hand gives what a read of the file alone gives.
        let json = |a: &[crate::affordance::Affordance]| serde_json::to_string(a).unwrap();
        crate::stream::collect_with(&ROOTLESS, &r, &Demand::RICH).unwrap();
        reads();
        let mut sink = crate::stream::CollectSink::default();
        ROOTLESS.stream(&r, &Demand::RICH, &mut sink).unwrap();
        let shared = ROOTLESS.store_affordances(&r).unwrap();
        assert_eq!(reads(), 1);
        let alone = ui_affordances(&ui).unwrap();
        assert_eq!(json(&shared), json(&alone));
        assert_eq!(alone.len(), 2);
    }

    #[test]
    fn roo_reuses_cline_parser_and_tags_harness() {
        let s = parse_history_str(
            "1717111111111",
            &fixture("api_conversation_history.json"),
            Harness::Roo,
            None,
        );
        assert_eq!(s.harness, Harness::Roo);
        assert_eq!(s.cwd.as_deref(), Some(std::path::Path::new("/home/dev/app")));
        assert_eq!(s.title.as_deref(), Some("Add a unit test"));
        // user, assistant, tool turn.
        assert_eq!(s.messages.len(), 3);
        assert_eq!(s.messages[2].role, Role::Tool);
    }

    /// Build a small Session, emit it to a unique temp dir, re-parse with this adapter, and assert
    /// message count + roles + tool blocks survive the round-trip.
    #[test]
    fn emit_round_trips_through_parser() {
        // ── build a small session ──
        let mut user = Message::new(Role::User);
        user.content = vec![Block::from_text("Please add a test.".into())];

        let mut asst = Message::new(Role::Assistant);
        asst.content = vec![
            Block::Thinking {
                text: "I should write a test.".into(),
                signature: Some("SIG==".into()),
                encrypted: None,
                redacted: false,
                extensions: Default::default(),
            },
            Block::from_text("On it.".into()),
            Block::ToolUse {
                id: "toolu_42".into(),
                name: "write_to_file".into(),
                input: serde_json::json!({ "path": "src/lib.rs" }).into(),
                namespace: None,
                extensions: Default::default(),
            },
        ];

        let mut tool = Message::new(Role::Tool);
        tool.content = vec![Block::ToolResult {
            tool_use_id: "toolu_42".into(),
            content: "File written.".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
            extensions: Default::default(),
        }];

        let session = Session {
            cwd: Some(PathBuf::from("/home/dev/app")),
            title: Some("Add a unit test".into()),
            messages: vec![user, asst, tool],
            ..Session::new("src", Harness::Roo)
        };

        // ── emit to a unique temp dir ──
        let out_dir = cv_testkit::temp_dir("roo-emit");

        let result = emit(&session, &out_dir, &crate::emit::EmitOptions::default()).expect("emit roo session");

        // emit returns the task DIR (tasks/<taskId>/), holding api_conversation_history.json.
        let task_dir = result.path.as_path();
        assert_eq!(
            task_dir.file_name().and_then(|s| s.to_str()),
            Some(result.new_id.as_str())
        );
        assert!(task_dir.join("api_conversation_history.json").exists());

        // ── re-parse with this adapter and assert structure survives ──
        let reparsed = parse_task_dir(task_dir, &result.new_id, Harness::Roo).expect("re-parse emitted roo session");

        assert_eq!(reparsed.harness, Harness::Roo);
        assert_eq!(reparsed.messages.len(), 3);
        assert_eq!(reparsed.messages[0].role, Role::User);
        assert_eq!(reparsed.messages[1].role, Role::Assistant);
        // tool_result-only user turn reclassified back to Tool.
        assert_eq!(reparsed.messages[2].role, Role::Tool);

        // cwd + title recovered from the injected <environment_details>/<task> wrappers.
        assert_eq!(reparsed.cwd.as_deref(), Some(std::path::Path::new("/home/dev/app")));
        assert_eq!(reparsed.title.as_deref(), Some("Add a unit test"));

        // Assistant blocks survived: thinking (with signature), text, tool_use.
        let asst = &reparsed.messages[1];
        assert!(matches!(
            &asst.content[0],
            Block::Thinking { signature: Some(sig), .. } if sig == "SIG=="
        ));
        assert!(asst
            .content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { name, .. } if name == "write_to_file")));

        // Tool result survived with its id + content.
        match &reparsed.messages[2].content[0] {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "toolu_42");
                assert!(content.contains("File written"));
                assert!(!is_error);
            }
            o => panic!("expected tool_result, got {o:?}"),
        }

        let _ = fs::remove_dir_all(&out_dir);
    }
}
