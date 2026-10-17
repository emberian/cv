//! The cybernetic layer: `cv hooks` installs five Claude Code hooks (idempotent, reversible) and
//! `cv hook <name>` is each hook's body — it reads the hook JSON on stdin and prints a line or two.
//!
//! Verified against the Claude Code 2.1.296 hooks docs: a `SessionStart` hook's
//! `hookSpecificOutput.additionalContext` is injected into the model's context (10K chars before it
//! spills to a file); `UserPromptSubmit` the same; `PreToolUse` answers
//! `permissionDecision: allow|deny|ask|defer`; `SubagentStart`/`SubagentStop` carry `agent_id`,
//! `agent_type` and (on stop) `agent_transcript_path` + `last_assistant_message`, and with
//! `async: true` nothing waits for them. Every hook is `cv hook <name>` with an absolute binary
//! path; the entries are recognized by that command shape, so `remove` deletes exactly them.
//!
//! Design constraint from the maintainer: agents set this up for themselves; not fancy; no daemon.
//! Every body reads the catalog or the index (never a fleet scan) and must finish well under 100 ms
//! on a cold cache — the latencies are measured in the lane report, not assumed.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};
use cv_core::ir::{home_rel, Harness, SessionRef};
use cv_core::lanes::LaneEvent;
use cv_core::sanitize::sanitize_line;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::short_id;
use crate::util::fmt_int;

/// The skill installed beside the hooks. It lives inside this crate (`crates/cv/skill/SKILL.md`) so the
/// published package carries it; a path outside the crate builds in the repo and nowhere else.
const SKILL: &str = include_str!("../../skill/SKILL.md");

/// A session whose transcript changed within this many minutes is *live* for the edit guard.
const LIVE_MINUTES: i64 = 30;
/// The start pack's hard cap (chars) — well under the harness's 10K inline limit.
const START_PACK_MAX: usize = 4_000;
/// How many of this directory's sessions the start pack lists.
const START_SESSIONS: usize = 6;
/// How far back from the end of a transcript the start pack looks for a `[branch sha]` line.
const COMMIT_TAIL_BYTES: u64 = 512 * 1024;
/// Above this size the compact hook reads the boundary from the transcript's tail instead of
/// streaming the whole file for its message index: the stream costs ~4.5 ms/MB (release, warm;
/// 16.5 MB measured at 70–90 ms), and the hook's budget is 100 ms cold. Below it the full
/// compaction detection runs.
const COMPACT_FULL_SCAN_MAX: u64 = 8 * 1024 * 1024;
/// The tail read on the large-transcript path: the boundary the harness just wrote sits a few
/// records from the end.
const COMPACT_TAIL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Subcommand)]
pub(crate) enum HooksCmd {
    /// Merge cv's five hooks into the harness's settings file and copy the `cv` skill beside them.
    /// Idempotent: a second run writes nothing.
    Install {
        /// The harness (`claude` is the one verified today).
        #[arg(default_value = "claude")]
        harness: String,
        /// `user` = `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR`); `project` = `./.claude/settings.json`.
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
    },
    /// Delete exactly the entries `install` wrote (and the copied skill), nothing else.
    Remove {
        #[arg(default_value = "claude")]
        harness: String,
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
    },
    /// Print what `install` would change: each entry present / missing, the skill, the settings path.
    Show {
        #[arg(default_value = "claude")]
        harness: String,
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
        /// Emit `{settings_path, installed, missing, skill_path, skill_present, before, after}`.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Scope {
    User,
    Project,
}

/// The five hook bodies. The name is what the installed command ends with (`cv hook compact`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, ValueEnum)]
pub(crate) enum HookName {
    /// `SessionStart` matcher `compact`: what was compacted and the three commands that read it back.
    Compact,
    /// `SessionStart` matcher `startup|resume`: a capped pack of this directory's last sessions.
    Start,
    /// `SubagentStart` / `SubagentStop` (async): a row in the lane ledger `cv lanes` reads.
    Lane,
    /// `UserPromptSubmit`: one line when decisions are owed to you, else nothing.
    Decisions,
    /// `PreToolUse` matcher `Edit|Write`: `ask` when another live session edited the file.
    Edit,
}

impl HookName {
    fn as_str(self) -> &'static str {
        match self {
            HookName::Compact => "compact",
            HookName::Start => "start",
            HookName::Lane => "lane",
            HookName::Decisions => "decisions",
            HookName::Edit => "edit",
        }
    }
    fn parse(s: &str) -> Option<HookName> {
        [
            HookName::Compact,
            HookName::Start,
            HookName::Lane,
            HookName::Decisions,
            HookName::Edit,
        ]
        .into_iter()
        .find(|n| n.as_str() == s)
    }
}

/// One settings entry: the event it hangs under, its matcher, the body, and whether it runs async.
struct Spec {
    event: &'static str,
    matcher: Option<&'static str>,
    name: HookName,
    asynchronous: bool,
}

/// Five hooks, six entries (`lane` serves both sub-agent events).
const SPECS: &[Spec] = &[
    Spec {
        event: "SessionStart",
        matcher: Some("compact"),
        name: HookName::Compact,
        asynchronous: false,
    },
    Spec {
        event: "SessionStart",
        matcher: Some("startup|resume"),
        name: HookName::Start,
        asynchronous: false,
    },
    Spec {
        event: "SubagentStart",
        matcher: None,
        name: HookName::Lane,
        asynchronous: true,
    },
    Spec {
        event: "SubagentStop",
        matcher: None,
        name: HookName::Lane,
        asynchronous: true,
    },
    Spec {
        event: "UserPromptSubmit",
        matcher: None,
        name: HookName::Decisions,
        asynchronous: false,
    },
    Spec {
        event: "PreToolUse",
        matcher: Some("Edit|Write"),
        name: HookName::Edit,
        asynchronous: false,
    },
];

// ───────────────────────────── settings file ─────────────────────────────

/// Claude Code's config dir: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn claude_config_dir() -> Option<PathBuf> {
    cv_core::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::util::dirs_home().map(|h| h.join(".claude")))
}

/// `(settings.json, skills/cv/SKILL.md)` for a scope.
fn paths(scope: Scope) -> Result<(PathBuf, PathBuf)> {
    let dir = match scope {
        Scope::User => claude_config_dir().context("no home directory: cannot locate ~/.claude")?,
        Scope::Project => std::env::current_dir()
            .context("reading the current directory")?
            .join(".claude"),
    };
    Ok((
        dir.join("settings.json"),
        dir.join("skills").join("cv").join("SKILL.md"),
    ))
}

fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON — not touching it", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_settings(path: &Path, v: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut text = serde_json::to_string_pretty(v)?;
    text.push('\n');
    publish_hook_file(path, &text)
}

fn publish_hook_file(path: &Path, text: &str) -> Result<()> {
    // Preserve dotfile symlinks, but replace the target inode so existing readers keep
    // complete old bytes and a failed write cannot truncate the user's settings.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    cv_core::publish::publish_reader(&target, "hooks", text.as_bytes(), cv_core::publish::Claim::Replace)
        .with_context(|| format!("writing {}", path.display()))
}

/// Shell-quote a path for a `command` string (shell form): bare when it is plain, single-quoted otherwise.
fn shell_quote(p: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | ':' | '~');
    if !p.is_empty() && p.chars().all(plain) {
        p.to_string()
    } else {
        format!("'{}'", p.replace('\'', "'\\''"))
    }
}

/// The `command` string for one hook: this binary, absolute, then `hook <name>`.
fn command_for(exe: &str, name: HookName) -> String {
    format!("{} hook {}", shell_quote(exe), name.as_str())
}

/// Is this handler one of ours? Recognized by shape: `<something>/cv hook <name>` (quotes allowed
/// around the program), so a reinstall from another binary path replaces rather than duplicates.
fn ours(handler: &Value) -> Option<HookName> {
    let cmd = handler.get("command")?.as_str()?.trim();
    let (prog, rest) = cmd.rsplit_once(" hook ")?;
    let name = HookName::parse(rest.trim())?;
    let prog = prog.trim().trim_matches(|c| c == '\'' || c == '"');
    let base = Path::new(prog).file_name()?.to_str()?;
    (base == "cv" || base == "cv.exe").then_some(name)
}

fn handler_for(exe: &str, spec: &Spec) -> Value {
    let mut h = Map::new();
    h.insert("type".into(), json!("command"));
    h.insert("command".into(), json!(command_for(exe, spec.name)));
    if spec.asynchronous {
        h.insert("async".into(), json!(true));
    } else {
        h.insert("timeout".into(), json!(10));
    }
    Value::Object(h)
}

/// Strip every handler of ours from `settings.hooks`, dropping groups and events left empty.
fn strip_ours(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for ev in SPECS.iter().map(|s| s.event).collect::<std::collections::BTreeSet<_>>() {
        if let Some(groups) = hooks.get_mut(ev).and_then(Value::as_array_mut) {
            for g in groups.iter_mut() {
                if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                    hs.retain(|h| ours(h).is_none());
                }
            }
            groups.retain(|g| {
                g.get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|hs| !hs.is_empty())
            });
            if groups.is_empty() {
                hooks.remove(ev);
            }
        }
    }
    if hooks.is_empty() {
        settings.as_object_mut().map(|o| o.remove("hooks"));
    }
}

/// `settings` with our six entries present (one group per spec, appended under its event).
fn with_ours(mut settings: Value, exe: &str) -> Value {
    strip_ours(&mut settings);
    let obj = settings.as_object_mut().expect("settings is an object");
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().expect("object");
    for spec in SPECS {
        let mut group = Map::new();
        if let Some(m) = spec.matcher {
            group.insert("matcher".into(), json!(m));
        }
        group.insert("hooks".into(), json!([handler_for(exe, spec)]));
        let groups = hooks.entry(spec.event).or_insert_with(|| json!([]));
        if !groups.is_array() {
            *groups = json!([]);
        }
        groups.as_array_mut().expect("array").push(Value::Object(group));
    }
    settings
}

/// Which of our entries are present in `settings`, by `(event, matcher, name)`, with the command found.
fn present(settings: &Value) -> HashMap<(&'static str, Option<&'static str>, HookName), String> {
    let mut found = HashMap::new();
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return found;
    };
    for spec in SPECS {
        let Some(groups) = hooks.get(spec.event).and_then(Value::as_array) else {
            continue;
        };
        for g in groups {
            let matcher = g.get("matcher").and_then(Value::as_str);
            if matcher != spec.matcher {
                continue;
            }
            for h in g.get("hooks").and_then(Value::as_array).into_iter().flatten() {
                if ours(h) == Some(spec.name) {
                    let cmd = h.get("command").and_then(Value::as_str).unwrap_or("").to_string();
                    found.insert((spec.event, spec.matcher, spec.name), cmd);
                }
            }
        }
    }
    found
}

fn require_claude(harness: &str) -> Result<()> {
    if harness != "claude" {
        bail!(
            "`cv hooks` knows Claude Code only (`claude`); {harness:?} has no verified hook contract yet — \
             the manual's Hooking a harness in page says what each harness offers"
        );
    }
    Ok(())
}

fn this_exe() -> Result<String> {
    let exe = std::env::current_exe().context("locating the cv binary")?;
    Ok(exe.to_string_lossy().into_owned())
}

pub(crate) fn cmd_hooks(cmd: HooksCmd) -> Result<()> {
    match cmd {
        HooksCmd::Install { harness, scope } => {
            require_claude(&harness)?;
            let (settings_path, skill_path) = paths(scope)?;
            let exe = this_exe()?;
            let before = read_settings(&settings_path)?;
            let after = with_ours(before.clone(), &exe);
            let mut changed = Vec::new();
            if after != before {
                write_settings(&settings_path, &after)?;
                changed.push(format!("{} entries → {}", SPECS.len(), home_rel(&settings_path)));
            }
            if std::fs::read_to_string(&skill_path).ok().as_deref() != Some(SKILL) {
                if let Some(dir) = skill_path.parent() {
                    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
                }
                publish_hook_file(&skill_path, SKILL)?;
                changed.push(format!("skill → {}", home_rel(&skill_path)));
            }
            if changed.is_empty() {
                println!(
                    "cv hooks: already installed ({} entries in {}); nothing written",
                    SPECS.len(),
                    home_rel(&settings_path)
                );
            } else {
                println!("cv hooks: installed {}", changed.join(" · "));
                println!("  (takes effect in new Claude Code sessions; `cv hooks show` lists the entries, `cv hooks remove` deletes them)");
            }
            Ok(())
        }
        HooksCmd::Remove { harness, scope } => {
            require_claude(&harness)?;
            let (settings_path, skill_path) = paths(scope)?;
            let before = read_settings(&settings_path)?;
            let n = present(&before).len();
            let mut after = before.clone();
            strip_ours(&mut after);
            let mut removed = Vec::new();
            if after != before {
                write_settings(&settings_path, &after)?;
                removed.push(format!("{n} entries from {}", home_rel(&settings_path)));
            }
            if skill_path.exists() {
                std::fs::remove_file(&skill_path).with_context(|| format!("removing {}", skill_path.display()))?;
                if let Some(dir) = skill_path.parent() {
                    let _ = std::fs::remove_dir(dir); // only if empty
                }
                removed.push(format!("skill {}", home_rel(&skill_path)));
            }
            if removed.is_empty() {
                println!(
                    "cv hooks: nothing of cv's in {}; nothing written",
                    home_rel(&settings_path)
                );
            } else {
                println!("cv hooks: removed {}", removed.join(" · "));
            }
            Ok(())
        }
        HooksCmd::Show { harness, scope, json } => {
            require_claude(&harness)?;
            let (settings_path, skill_path) = paths(scope)?;
            let exe = this_exe()?;
            let before = read_settings(&settings_path)?;
            let after = with_ours(before.clone(), &exe);
            let have = present(&before);
            let skill_present = std::fs::read_to_string(&skill_path).ok().as_deref() == Some(SKILL);
            if json {
                let (mut installed, mut missing) = (Vec::new(), Vec::new());
                for s in SPECS {
                    let row = json!({"event": s.event, "matcher": s.matcher, "hook": s.name.as_str(),
                        "command": command_for(&exe, s.name)});
                    match have.get(&(s.event, s.matcher, s.name)) {
                        Some(cmd) if *cmd == command_for(&exe, s.name) => installed.push(row),
                        _ => missing.push(row),
                    }
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "settings_path": settings_path, "installed": installed, "missing": missing,
                        "skill_path": skill_path, "skill_present": skill_present,
                        "before": before, "after": after, "would_change": after != before || !skill_present,
                    }))?
                );
                return Ok(());
            }
            println!("# cv hooks — {} ({:?} scope)\n", home_rel(&settings_path), scope);
            for s in SPECS {
                let want = command_for(&exe, s.name);
                let mark = match have.get(&(s.event, s.matcher, s.name)) {
                    Some(cmd) if *cmd == want => "✓",
                    Some(_) => "~", // ours, another binary path: install replaces it
                    None => "+",
                };
                let matcher = s.matcher.map(|m| format!(" [{m}]")).unwrap_or_default();
                println!("  {mark} {:<17}{matcher:<18} {want}", s.event);
            }
            println!(
                "  {} skill {}",
                if skill_present { "✓" } else { "+" },
                home_rel(&skill_path)
            );
            println!(
                "\n{}",
                if after == before && skill_present {
                    "installed; `cv hooks install claude` would write nothing".to_string()
                } else {
                    "✓ present · + `cv hooks install claude` adds · ~ replaces (another cv binary's path)".to_string()
                }
            );
            Ok(())
        }
    }
}

// ───────────────────────────── doctor's line ─────────────────────────────

/// The compaction knobs Claude Code honours, read from the user and project settings (project wins)
/// and the process environment (wins over a settings `env` block). `None` = unset.
pub(crate) struct Policy {
    pub auto_compact_enabled: Option<bool>,
    pub idle_compaction: Option<bool>,
    pub env: Vec<(&'static str, Option<String>)>,
}

const POLICY_ENV: &[&str] = &[
    "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "DISABLE_AUTO_COMPACT",
];

/// `(installed entries, settings path)` per scope, and the policy in force — what `cv doctor` prints.
pub(crate) fn status() -> (Vec<(Scope, usize, PathBuf)>, Policy) {
    let mut scopes = Vec::new();
    let mut auto = None;
    let mut idle = None;
    let mut env_block: HashMap<String, String> = HashMap::new();
    for scope in [Scope::User, Scope::Project] {
        let Ok((path, _)) = paths(scope) else { continue };
        let settings = read_settings(&path).unwrap_or(json!({}));
        scopes.push((scope, present(&settings).len(), path));
        if let Some(b) = settings.get("autoCompactEnabled").and_then(Value::as_bool) {
            auto = Some(b);
        }
        if let Some(b) = settings.get("idleCompaction").and_then(Value::as_bool) {
            idle = Some(b);
        }
        if let Some(env) = settings.get("env").and_then(Value::as_object) {
            for (k, v) in env {
                if let Some(s) = v.as_str() {
                    env_block.insert(k.clone(), s.to_string());
                }
            }
        }
    }
    let env = POLICY_ENV
        .iter()
        .map(|k| (*k, cv_core::env::var(k).ok().or_else(|| env_block.get(*k).cloned())))
        .collect();
    (
        scopes,
        Policy {
            auto_compact_enabled: auto,
            idle_compaction: idle,
            env,
        },
    )
}

/// The two lines `cv doctor` prints under its header.
pub(crate) fn doctor_lines() -> Vec<String> {
    let (scopes, policy) = status();
    let total = SPECS.len();
    let installed: Vec<String> = scopes
        .iter()
        .filter(|(_, n, _)| *n > 0)
        .map(|(scope, n, path)| format!("{n}/{total} in {} ({scope:?})", home_rel(path)))
        .collect();
    let hooks = if installed.is_empty() {
        "not installed — `cv hooks install claude` wires compaction, session start, lanes, decisions and the edit guard"
            .to_string()
    } else {
        installed.join(", ")
    };
    let onoff = |v: Option<bool>, default_on: bool| match v {
        Some(true) => "true".to_string(),
        Some(false) => "false".to_string(),
        None => format!("unset (default {})", if default_on { "on" } else { "off" }),
    };
    let mut knobs = vec![
        format!("autoCompactEnabled {}", onoff(policy.auto_compact_enabled, true)),
        format!("idleCompaction {}", onoff(policy.idle_compaction, false)),
    ];
    knobs.extend(policy.env.iter().map(|(k, v)| match v {
        Some(v) => format!("{k}={}", sanitize_line(v)),
        None => format!("{k} unset"),
    }));
    vec![
        format!("Hooks:       {hooks}"),
        format!("Policy:      {}", knobs.join(" · ")),
    ]
}

pub(crate) fn doctor_json() -> Value {
    let (scopes, policy) = status();
    json!({
        "installed": scopes.iter().map(|(scope, n, path)| json!({
            "scope": format!("{scope:?}").to_lowercase(), "entries": n, "of": SPECS.len(), "settings_path": path
        })).collect::<Vec<_>>(),
        "policy": {
            "auto_compact_enabled": policy.auto_compact_enabled,
            "idle_compaction": policy.idle_compaction,
            "env": policy.env.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<String, Value>>(),
        }
    })
}

// ───────────────────────────── the hook bodies ─────────────────────────────

/// The fields the five bodies read from the hook JSON (every other field is ignored).
#[derive(Deserialize, Default)]
struct Input {
    #[serde(default)]
    session_id: String,
    transcript_path: Option<PathBuf>,
    cwd: Option<PathBuf>,
    hook_event_name: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    agent_transcript_path: Option<PathBuf>,
    last_assistant_message: Option<String>,
    tool_input: Option<Value>,
}

/// `cv hook <name>`: read stdin, run the body, print its one-line JSON (or nothing). A body that
/// fails prints its error on stderr and still exits 0 — a hook must never block the harness. A
/// terminal on stdin means no harness is calling: usage, exit 2, instead of waiting for JSON.
pub(crate) fn cmd_hook(name: HookName) -> Result<()> {
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return crate::util::usage(format!(
            "cv hook {} reads Claude Code's hook JSON on stdin, and stdin is a terminal; \
             `cv hooks install claude` writes the settings entries that pipe it",
            name.as_str()
        ));
    }
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let input: Input = match serde_json::from_str(&raw) {
        Ok(i) => i,
        Err(e) => {
            eprintln!(
                "cv hook {}: stdin is not the hook JSON ({e}); nothing to do",
                name.as_str()
            );
            return Ok(());
        }
    };
    let out = match name {
        HookName::Compact => hook_compact(&input),
        HookName::Start => hook_start(&input),
        HookName::Lane => hook_lane(&input),
        HookName::Decisions => hook_decisions(&input),
        HookName::Edit => hook_edit(&input),
    };
    match out {
        Ok(Some(v)) => println!("{v}"),
        Ok(None) => {}
        Err(e) => eprintln!("cv hook {}: {e:#}", name.as_str()),
    }
    Ok(())
}

fn context(event: &str, text: String) -> Value {
    json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}})
}

fn claude_ref(id: &str, path: &Path) -> SessionRef {
    SessionRef {
        id: id.to_string(),
        harness: Harness::Claude,
        path: path.to_path_buf(),
        cwd: None,
        title: None,
        created_at: None,
        updated_at: None,
        message_count: 0,
    }
}

/// Read the last [`COMPACT_TAIL_BYTES`] of `path`.
fn tail_text(path: &Path, bytes: u64) -> Option<String> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The newest `compact_boundary` record in a transcript's tail: `(trigger, preTokens)`. Reads
/// one line's JSON, never the file — the large-transcript path of [`hook_compact`].
fn last_boundary_in_tail(path: &Path) -> Option<(Option<String>, Option<u64>)> {
    let text = tail_text(path, COMPACT_TAIL_BYTES)?;
    let v: Value = text
        .lines()
        .rev()
        .filter(|l| l.contains("\"compact_boundary\""))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("subtype").and_then(Value::as_str) == Some("compact_boundary"))?;
    let meta = v.get("compactMetadata");
    Some((
        meta.and_then(|m| m.get("trigger"))
            .and_then(Value::as_str)
            .map(str::to_string),
        meta.and_then(|m| m.get("preTokens")).and_then(Value::as_u64),
    ))
}

fn not_recorded_yet(id: &str) -> Value {
    context(
        "SessionStart",
        format!(
            "context compacted; the transcript has not recorded the boundary yet — `cv show {id} --kind compaction_boundary,compaction_summary` \
             lists every boundary once it has, `cv show {id} --pre-compaction N` reads the span the Nth discarded."
        ),
    )
}

/// `SessionStart` (`compact`): the boundary the harness just wrote and the commands that read what
/// it discarded. A transcript up to [`COMPACT_FULL_SCAN_MAX`] is streamed the way `cv show --kind
/// compaction_boundary` does it (exact message index and ordinal); a larger one is read from its tail (trigger and
/// size exact, the index left to that view), so the hook stays inside its budget however
/// long the session has run.
fn hook_compact(input: &Input) -> Result<Option<Value>> {
    let Some(path) = input.transcript_path.as_deref() else {
        return Ok(None);
    };
    let id = short_id(&input.session_id);
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > COMPACT_FULL_SCAN_MAX {
        // A tail without a boundary (one far back in a long transcript) still gets the pointers.
        let what = match last_boundary_in_tail(path) {
            Some((trigger, pre_tokens)) => format!(
                " ({}, {})",
                trigger.as_deref().unwrap_or("compaction"),
                pre_tokens
                    .map(|t| format!("{} tokens", fmt_int(t)))
                    .unwrap_or_else(|| "size unrecorded".into())
            ),
            None => String::new(),
        };
        return Ok(Some(context(
            "SessionStart",
            format!(
                "context compacted{what}. `cv show {id} --kind compaction_boundary,compaction_summary` lists this session's \
                 boundaries with their message indexes; `cv show {id} --pre-compaction N` reads what the Nth discarded (`cv show \
                 {id} --kind prompt,answer --pre-compaction N` just its prompts); `cv rewind {id} --at <msg>` resumes the agent as of a message. \
                 {AFFORDANCE_POINTER}",
                AFFORDANCE_POINTER = affordance_pointer(&id)
            ),
        )));
    }
    let comps = cv_core::compaction::detect(&claude_ref(&input.session_id, path), false)?;
    let Some(last) = comps.last() else {
        return Ok(Some(not_recorded_yet(&id)));
    };
    let k = comps.len();
    let trigger = last.trigger.as_deref().unwrap_or("compaction");
    let pre = last
        .pre_tokens
        .map(|t| format!("{} tokens", fmt_int(t)))
        .unwrap_or_else(|| "size unrecorded".into());
    let idx = last.boundary_msg_idx;
    let mut text = format!(
        "context compacted ({trigger}, {pre}) at message {idx} — compaction #{k} of this session. \
         `cv show {id} --pre-compaction {k}` reads what was lost (`cv show {id} --kind prompt,answer --pre-compaction {k}` just its prompts)"
    );
    if idx > 0 {
        text.push_str(&format!(
            "; `cv rewind {id} --at {}` resumes the agent as of before it.",
            idx - 1
        ));
    } else {
        text.push('.');
    }
    text.push(' ');
    text.push_str(&affordance_pointer(&id));
    Ok(Some(context("SessionStart", text)))
}

/// The one command that reads back what a compaction summary tends to flatten: the session's plan
/// (with the task list beside the transcript), its schedules and its background jobs.
fn affordance_pointer(id: &str) -> String {
    format!("`cv show {id} --kind plan,schedule,background_job` lists the plan, schedules and background jobs this session holds.")
}

/// The last `[branch sha] subject` a transcript's tail recorded (a `git commit` result), reading at
/// most [`COMMIT_TAIL_BYTES`] from the end — never a parse of the whole file.
fn last_commit(path: &Path) -> Option<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r#"\[([^\]\s\\"]+)(?: \([^)]*\))? ([0-9a-f]{7,40})\] "#).expect("static regex")
    });
    let text = tail_text(path, COMMIT_TAIL_BYTES)?;
    re.captures_iter(&text)
        .last()
        .map(|c| c[2][..7.min(c[2].len())].to_string())
}

fn ago(t: Option<chrono::DateTime<chrono::Utc>>) -> String {
    t.map(|t| format!("{} ago", cv_core::task::age_short(t, chrono::Utc::now())))
        .unwrap_or_else(|| "undated".into())
}

/// `(who, decisions owed)` from the task log, under the identity `cv task inbox` would use.
fn owed_decisions() -> Result<(String, usize)> {
    let who = cv_core::task::actor(None, "cv");
    let outcome = cv_core::task::replay()?;
    let n = cv_core::task::inbox(&outcome.model, &who)
        .iter()
        .filter(|e| matches!(e.reason, cv_core::task::InboxReason::DecisionOwed))
        .count();
    Ok((who, n))
}

/// `SessionStart` (`startup|resume`): this directory's last sessions from the catalog as it stands
/// (ids, ages, titles, last commit shas) and the decisions owed — capped, nothing else.
fn hook_start(input: &Input) -> Result<Option<Value>> {
    let cwd = match &input.cwd {
        Some(c) => c.clone(),
        None => std::env::current_dir()?,
    };
    // One canonicalize for the hook's cwd; the catalog's cwds are compared as recorded (a
    // per-row canonicalize is thousands of syscalls on a real store, for nothing).
    let canon = std::fs::canonicalize(&cwd).unwrap_or_else(|_| cwd.clone());
    let mut rows: Vec<SessionRef> = cv_core::cataloged_sessions()
        .into_iter()
        .filter(|r| r.id != input.session_id && !r.id.starts_with("agent-"))
        .filter(|r| r.cwd.as_deref().is_some_and(|c| c == cwd || c == canon))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at)));
    rows.truncate(START_SESSIONS);
    let (who, owed) = owed_decisions().unwrap_or_else(|_| ("cv".into(), 0));
    if rows.is_empty() && owed == 0 {
        return Ok(None);
    }
    let mut lines = Vec::new();
    if !rows.is_empty() {
        lines.push(format!(
            "cv: {} earlier session(s) in this directory (catalog as of the last `cv ls`; newest first). \
             `cv show <id>` reads one; `cv pack \"<task>\"` pulls the relevant parts of several into context.",
            rows.len()
        ));
        for r in &rows {
            // A catalog title that is harness markup (`<local-command-caveat>…`) is no title.
            let title = r
                .title
                .as_deref()
                .filter(|t| !t.trim_start().starts_with('<'))
                .map(|t| cv_core::ir::truncate(&sanitize_line(t), 60))
                .unwrap_or_else(|| "(untitled)".into());
            let commit = last_commit(&r.path)
                .map(|s| format!(" · last commit {s}"))
                .unwrap_or_default();
            let id = match r.harness {
                Harness::Claude => short_id(&r.id),
                h => format!("{h}:{}", short_id(&r.id)),
            };
            lines.push(format!(
                "- {id} · {} · {} msgs · {title}{commit}",
                ago(r.updated_at.or(r.created_at)),
                r.message_count
            ));
        }
    }
    if owed > 0 {
        lines.push(format!(
            "- {owed} decision(s) owed to {who}: `cv task inbox` lists them."
        ));
    }
    let mut text = lines.join("\n");
    while text.chars().count() > START_PACK_MAX && lines.len() > 1 {
        lines.pop();
        text = lines.join("\n");
    }
    Ok(Some(context("SessionStart", text)))
}

/// `SubagentStart` / `SubagentStop`: one row in the lane ledger; no output, no model action.
fn hook_lane(input: &Input) -> Result<Option<Value>> {
    let event = match input.hook_event_name.as_deref() {
        Some("SubagentStart") => "start",
        Some("SubagentStop") => "stop",
        _ => return Ok(None),
    };
    let Some(agent_id) = input.agent_id.as_deref() else {
        return Ok(None);
    };
    let agent_id = agent_id.strip_prefix("agent-").unwrap_or(agent_id).to_string();
    let expand = |p: &Path| -> PathBuf {
        match (p.strip_prefix("~"), crate::util::dirs_home()) {
            (Ok(rest), Some(h)) => h.join(rest),
            _ => p.to_path_buf(),
        }
    };
    let transcript_path = input.transcript_path.as_deref().map(expand);
    let agent_transcript_path = input.agent_transcript_path.as_deref().map(expand).or_else(|| {
        transcript_path
            .as_deref()
            .and_then(cv_core::harness::claude::sidecar::subagents_dir)
            .map(|d| d.join(format!("agent-{agent_id}.jsonl")))
    });
    let last_text = input
        .last_assistant_message
        .as_deref()
        .and_then(|t| t.lines().find(|l| !l.trim().is_empty()))
        .map(|l| cv_core::ir::truncate(l.trim(), 200));
    cv_core::lanes::append_lane_event(&LaneEvent {
        ts: chrono::Utc::now(),
        event: event.into(),
        session_id: input.session_id.clone(),
        agent_id,
        agent_type: input.agent_type.clone(),
        transcript_path,
        agent_transcript_path,
        cwd: input.cwd.clone(),
        last_text,
    })
    .context("appending to the lane ledger")?;
    Ok(None)
}

/// `UserPromptSubmit`: one line when decisions are owed, else nothing at all.
fn hook_decisions(_input: &Input) -> Result<Option<Value>> {
    let (who, n) = owed_decisions()?;
    if n == 0 {
        return Ok(None);
    }
    Ok(Some(context(
        "UserPromptSubmit",
        format!("cv: {n} decision(s) owed to {who} — `cv task inbox` lists them; `cv task resolve <id> --accept-default` or `--choice <option>` answers one."),
    )))
}

/// The transcript behind an indexed session, without a scan: the catalog for a top-level id, the
/// parent's `subagents/` dir for an `agent-…` id.
fn transcript_of(t: &cv_core::events::TouchedSession) -> Option<PathBuf> {
    if let Some(bare) = t.session_id.strip_prefix("agent-") {
        let parent = cv_core::catalog::lookup(t.parent_id.as_deref()?, None)
            .into_iter()
            .next()?;
        let dir = cv_core::harness::claude::sidecar::subagents_dir(&parent.path)?;
        let direct = dir.join(format!("agent-{bare}.jsonl"));
        if direct.exists() {
            return Some(direct);
        }
        let wf = t.workflow.as_deref()?;
        return Some(dir.join("workflows").join(wf).join(format!("agent-{bare}.jsonl")));
    }
    cv_core::catalog::lookup(&t.session_id, None)
        .into_iter()
        .find(|r| r.id == t.session_id)
        .map(|r| r.path)
}

fn minutes_since_modified(path: &Path) -> Option<i64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(std::time::SystemTime::now().duration_since(m).ok()?.as_secs() as i64 / 60)
}

fn is_current_editor(t: &cv_core::events::TouchedSession, session_id: &str, agent_id: Option<&str>) -> bool {
    t.harness == "claude"
        && t.parent_id.as_deref().unwrap_or(&t.session_id) == session_id
        && t.agent_id.as_deref() == agent_id
}

/// `PreToolUse` (`Edit|Write`): `ask` when another live session edited this path — the edit
/// index (`cv touched`) names the sessions, the catalog names their transcripts, one `stat` each
/// says which are live. Nothing printed = allow.
fn hook_edit(input: &Input) -> Result<Option<Value>> {
    let Some(file) = input
        .tool_input
        .as_ref()
        .and_then(|t| t.get("file_path").or_else(|| t.get("notebook_path")))
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let abs = if Path::new(file).is_absolute() {
        PathBuf::from(file)
    } else {
        input
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
            .join(file.trim_start_matches("./"))
    };
    let my_agent = input
        .agent_id
        .as_deref()
        .map(|a| a.strip_prefix("agent-").unwrap_or(a).to_string());
    let mut live: Vec<(String, i64)> = Vec::new();
    for t in cv_core::events::sessions_editing(&abs.to_string_lossy()) {
        if is_current_editor(&t, &input.session_id, my_agent.as_deref()) {
            continue;
        }
        let Some(path) = transcript_of(&t) else { continue };
        if let Some(m) = minutes_since_modified(&path).filter(|m| *m <= LIVE_MINUTES) {
            live.push((t.session_id.clone(), m));
        }
    }
    if live.is_empty() {
        return Ok(None);
    }
    live.sort_by_key(|(_, m)| *m);
    let (first, mins) = &live[0];
    let more = match live.len() {
        1 => String::new(),
        n => format!(" (and {} more live session{})", n - 1, if n == 2 { "" } else { "s" }),
    };
    let shown = home_rel(&abs);
    let reason = format!(
        "cv: {shown} was edited by live session {} ({mins} min ago){more}. `cv show {} --last 10` shows its hand; proceed only if the edits do not collide.",
        short_id(first),
        short_id(first)
    );
    Ok(Some(json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "ask",
        "permissionDecisionReason": reason,
    }})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_install_keeps_existing_readers_on_the_previous_files() {
        use std::io::Read;
        let _world = crate::test_world("hooks-atomic");
        let (settings, skill) = paths(Scope::User).unwrap();
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&settings, "{}").unwrap();
        std::fs::write(&skill, "previous skill").unwrap();
        let mut previous_settings = std::fs::File::open(&settings).unwrap();
        let mut previous_skill = std::fs::File::open(&skill).unwrap();
        cmd_hooks(HooksCmd::Install {
            harness: "claude".into(),
            scope: Scope::User,
        })
        .unwrap();
        let mut held = String::new();
        previous_settings.read_to_string(&mut held).unwrap();
        assert_eq!(held, "{}");
        held.clear();
        previous_skill.read_to_string(&mut held).unwrap();
        assert_eq!(held, "previous skill");
        assert_eq!(
            read_settings(&settings).unwrap(),
            with_ours(json!({}), &this_exe().unwrap())
        );
        assert_eq!(std::fs::read_to_string(skill).unwrap(), SKILL);
    }

    #[test]
    fn edit_conflicts_distinguish_parent_sibling_and_reused_agent_identities() {
        let mut touched = cv_core::events::TouchedSession {
            harness: "claude".into(),
            session_id: "parent".into(),
            title: None,
            edits: 1,
            reads: 0,
            last_ts: None,
            parent_id: None,
            agent_id: None,
            workflow: None,
        };
        assert!(is_current_editor(&touched, "parent", None));
        assert!(
            !is_current_editor(&touched, "parent", Some("self")),
            "a child and parent are separate writers"
        );
        touched.session_id = "agent-self".into();
        touched.parent_id = Some("parent".into());
        touched.agent_id = Some("self".into());
        assert!(is_current_editor(&touched, "parent", Some("self")));
        assert!(!is_current_editor(&touched, "parent", Some("sibling")));
        assert!(!is_current_editor(&touched, "parent", None));
        assert!(!is_current_editor(&touched, "another-parent", Some("self")));
        touched.harness = "codex".into();
        assert!(!is_current_editor(&touched, "parent", Some("self")));
    }

    #[test]
    fn install_is_idempotent_and_remove_leaves_the_rest_untouched() {
        let theirs = json!({
            "theme": "dark",
            "hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "/usr/bin/notify"}]}]
            }
        });
        let once = with_ours(theirs.clone(), "/opt/cv");
        let twice = with_ours(once.clone(), "/opt/cv");
        assert_eq!(once, twice, "a second install changes nothing");
        assert_eq!(present(&once).len(), SPECS.len());
        // Theirs survived, in place.
        assert_eq!(once["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(once["hooks"]["PreToolUse"][1]["matcher"], "Edit|Write");
        assert_eq!(once["hooks"]["SubagentStop"][0]["hooks"][0]["async"], true);
        assert!(once["hooks"]["SessionStart"][0]["hooks"][0]["async"].is_null());
        // A reinstall from another binary path replaces ours rather than adding a seventh.
        let moved = with_ours(once.clone(), "/usr/local/bin/cv");
        assert_eq!(present(&moved).len(), SPECS.len());
        assert_eq!(moved["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        let mut back = moved;
        strip_ours(&mut back);
        assert_eq!(back, theirs, "remove restores the original exactly");
        // Nothing of ours in an empty file leaves no `hooks` key behind.
        let mut empty = with_ours(json!({}), "/opt/cv");
        strip_ours(&mut empty);
        assert_eq!(empty, json!({}));
    }

    #[test]
    fn ours_recognizes_the_command_shape_only() {
        assert_eq!(
            ours(&json!({"command": "/opt/cv hook compact"})),
            Some(HookName::Compact)
        );
        assert_eq!(
            ours(&json!({"command": "'/Applications/My Tools/cv' hook lane"})),
            Some(HookName::Lane)
        );
        assert_eq!(ours(&json!({"command": "/opt/cv hook nope"})), None);
        assert_eq!(ours(&json!({"command": "/opt/other hook compact"})), None);
        assert_eq!(ours(&json!({"command": "cv-mcp"})), None);
        assert_eq!(shell_quote("/a b/cv"), "'/a b/cv'");
        assert_eq!(shell_quote("/usr/local/bin/cv"), "/usr/local/bin/cv");
    }

    #[test]
    fn the_large_transcript_path_reads_the_newest_boundary_from_the_tail() {
        let dir = std::env::temp_dir().join(format!("cv-hooks-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        let b = |trig: &str, pre: u64| {
            json!({"type": "system", "subtype": "compact_boundary", "compactMetadata": {"trigger": trig, "preTokens": pre}})
                .to_string()
        };
        // A user turn that merely quotes the word is not a boundary record.
        let quote = json!({"type": "user", "message": {"content": "grep \"compact_boundary\" x"}}).to_string();
        std::fs::write(&p, format!("{}\n{}\n{quote}\n", b("manual", 1), b("auto", 180_412))).unwrap();
        assert_eq!(last_boundary_in_tail(&p), Some((Some("auto".into()), Some(180_412))));
        std::fs::write(&p, format!("{}\n{quote}\n", b("auto", 5))).unwrap();
        assert_eq!(last_boundary_in_tail(&p), Some((Some("auto".into()), Some(5))));
        std::fs::write(&p, format!("{quote}\n")).unwrap();
        assert_eq!(last_boundary_in_tail(&p), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn last_commit_reads_the_tail_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("cv-hooks-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        std::fs::write(
            &p,
            concat!(
                "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"[main 3e6a269] docs\\n 1 file\"}]}}\n",
                "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"[p3-hooks 0abec0fd1] later\\n\"}]}}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"see [1] and [2]\"}]}}\n",
            ),
        )
        .unwrap();
        assert_eq!(last_commit(&p).as_deref(), Some("0abec0f"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
