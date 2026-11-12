//! The cybernetic layer: `cv hooks` installs five Claude Code or Codex hooks (idempotent, reversible) and
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

// Hook commands do local bookkeeping; bound how long they can hold up the harness.
const HOOK_TIMEOUT_SECS: u64 = 10;

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};
use cv_core::hooks::Event;
use cv_core::ir::{home_rel, Harness, SessionRef};
use cv_core::lanes::LaneEvent;
use cv_core::sanitize::sanitize_line;
use serde_json::{json, Map, Value};

use crate::util::short_id;
use crate::util::{fmt_int, shell_quote};

/// The skill installed beside the hooks. It lives inside this crate (`crates/cv/skill/SKILL.md`) so the
/// published package carries it; a path outside the crate builds in the repo and nowhere else.
const SKILL: &str = include_str!("../../skill/SKILL.md");

/// A session whose transcript changed within this many minutes is *live* for the edit guard.
const LIVE_MINUTES: i64 = 30;
/// How many of this directory's sessions the start pack lists.
const START_SESSIONS: usize = 6;
/// The hooks' one latency budget, in transcript bytes. A hook answers inside the harness's turn
/// (100 ms cold), and streaming a transcript costs ~4.5 ms/MB (release, warm; 16.5 MB measured at
/// 70–90 ms), so a hook reads at most this much and its output names what it left unread. The
/// compact hook streams a transcript up to it whole, else reads its last this-many bytes; the
/// start pack splits it across the sessions it lists.
const HOOK_READ_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Subcommand)]
pub(crate) enum HooksCmd {
    /// Merge cv's five hooks into the harness's settings file and copy the `cv` skill beside them.
    /// Idempotent: a second run writes nothing.
    Install {
        /// The harness (`claude` or `codex`).
        #[arg(default_value = "claude")]
        harness: String,
        /// `user` writes the harness home; `project` writes `.claude/settings.json` or `.codex/hooks.json`.
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
        /// Install only the retired-agent guard (`cv hook retired`, PreToolUse on every tool):
        /// it refuses each tool call of an agent `cv distill --agent-of` replaced, and each
        /// agent message addressed to one, naming the successor.
        #[arg(long)]
        retired: bool,
        /// Install only the receipt log (`cv hook receipt`, PreToolUse and PostToolUse on every
        /// tool): each call's input digest is appended to a hash-chained log in `state.db`
        /// before the call runs; `CV_RECEIPT_STRICT=1` refuses a call it cannot log.
        #[arg(long, conflicts_with = "retired")]
        receipts: bool,
    },
    /// Delete exactly the entries `install` wrote (and the copied skill), nothing else.
    /// `--retired` deletes only the retired-agent guard.
    Remove {
        #[arg(default_value = "claude")]
        harness: String,
        #[arg(long, value_enum, default_value_t = Scope::User)]
        scope: Scope,
        #[arg(long)]
        retired: bool,
        /// Delete only the receipt log's entries.
        #[arg(long, conflicts_with = "retired")]
        receipts: bool,
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
    /// `PreToolUse`, every tool (opt-in, `cv hooks install --retired`): `deny` a retired agent's
    /// own calls and a `SendMessage` to one, naming its successor.
    Retired,
    /// `PreToolUse` and `PostToolUse`, every tool (opt-in, `cv hooks install --receipts`): append
    /// the call to the session's hash-chained receipt log and fsync it BEFORE the tool runs
    /// (commit-before-forward), then its result's digest after.
    Receipt,
}

impl HookName {
    fn as_str(self) -> &'static str {
        match self {
            HookName::Compact => "compact",
            HookName::Start => "start",
            HookName::Lane => "lane",
            HookName::Decisions => "decisions",
            HookName::Edit => "edit",
            HookName::Retired => "retired",
            HookName::Receipt => "receipt",
        }
    }
    fn parse(s: &str) -> Option<HookName> {
        [
            HookName::Compact,
            HookName::Start,
            HookName::Lane,
            HookName::Decisions,
            HookName::Edit,
            HookName::Retired,
            HookName::Receipt,
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

/// The retired-agent guard: not in [`SPECS`] (a hook on every tool call is opt-in), Claude only.
const RETIRED: Spec = Spec {
    event: "PreToolUse",
    matcher: None,
    name: HookName::Retired,
    asynchronous: false,
};

/// The receipt log: not in [`SPECS`] (a hook on every tool call is opt-in), Claude only.
const RECEIPTS: [Spec; 2] = [
    Spec {
        event: "PreToolUse",
        matcher: None,
        name: HookName::Receipt,
        asynchronous: false,
    },
    Spec {
        event: "PostToolUse",
        matcher: None,
        name: HookName::Receipt,
        asynchronous: false,
    },
];

// Older released Codex builds do not expose the newer Claude-compatible matcher aliases.
const CODEX_EDIT: Spec = Spec {
    event: "PreToolUse",
    matcher: Some("apply_patch"),
    name: HookName::Edit,
    asynchronous: false,
};
fn specs(harness: Harness) -> impl Iterator<Item = &'static Spec> {
    let edit = host(harness).and_then(|h| h.edit);
    SPECS.iter().map(move |spec| match edit {
        Some(edit) if spec.name == HookName::Edit => edit,
        _ => spec,
    })
}

// ───────────────────────────── settings file ─────────────────────────────

/// A harness with a verified hook contract, as `cv hooks` writes and reads its settings.
struct Host {
    harness: Harness,
    /// The variable that moves the user-scope config dir, and the dir's name under the home (and
    /// under a project).
    config_var: &'static str,
    dir: &'static str,
    /// The file the hook entries live in.
    settings: &'static str,
    /// The Edit hook's entry where it differs from [`SPECS`]'.
    edit: Option<&'static Spec>,
    /// A handler runs only once the harness's own config trusts it (Codex's `trusted_hash`).
    trust: bool,
    /// The settings also hold the compaction policy `cv doctor` reports.
    policy: bool,
    /// PreToolUse/PostToolUse payloads carry `tool_use_id` for the receipt log.
    receipts: bool,
    /// PreToolUse payloads carry sub-agent `agent_id` for the retired-agent guard.
    retired_guard: bool,
}

/// The one per-harness table of `cv hooks`: `None` for a harness with no verified hook contract.
fn host(h: Harness) -> Option<Host> {
    match h {
        Harness::Claude => Some(Host {
            harness: h,
            config_var: "CLAUDE_CONFIG_DIR",
            dir: ".claude",
            settings: "settings.json",
            edit: None,
            trust: false,
            policy: true,
            receipts: true,
            retired_guard: true,
        }),
        Harness::Codex => Some(Host {
            harness: h,
            config_var: "CODEX_HOME",
            dir: ".codex",
            settings: "hooks.json",
            edit: Some(&CODEX_EDIT),
            trust: true,
            policy: false,
            receipts: true,
            retired_guard: true,
        }),
        Harness::Grok
        | Harness::OpenCode
        | Harness::Gemini
        | Harness::Antigravity
        | Harness::Hermes
        | Harness::OpenClaw
        | Harness::Cursor
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Qwen
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => None,
    }
}

/// Every harness `cv hooks` can install into, in [`Harness::ALL`] order.
fn hosts() -> impl Iterator<Item = Host> {
    Harness::ALL.iter().filter_map(|h| host(*h))
}

/// Whether `harness`'s handlers need the harness's own trust ([`Host::trust`]).
fn needs_trust(harness: Harness) -> bool {
    host(harness).is_some_and(|h| h.trust)
}

impl Host {
    /// The user-scope config dir: the host's variable (`$CLAUDE_CONFIG_DIR`, `$CODEX_HOME`), else
    /// its dir under the home.
    fn user_dir(&self) -> Option<PathBuf> {
        cv_core::env::var_os(self.config_var)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| crate::util::dirs_home().map(|h| h.join(self.dir)))
    }
}

/// `(settings file, skills/cv/SKILL.md)` for a harness and scope.
fn paths(harness: Harness, scope: Scope) -> Result<(PathBuf, PathBuf)> {
    let host = host(harness).with_context(|| format!("{harness} has no verified hook contract"))?;
    let dir = match scope {
        Scope::User => host
            .user_dir()
            .with_context(|| format!("no home directory: cannot locate ~/{}", host.dir))?,
        Scope::Project => std::env::current_dir()
            .context("reading the current directory")?
            .join(host.dir),
    };
    Ok((dir.join(host.settings), dir.join("skills").join("cv").join("SKILL.md")))
}

// ───────────────────────────── Codex trust ─────────────────────────────
//
// Codex runs a `hooks.json` handler only when its identity hash is recorded under
// `[hooks.state."<hooks.json>:<event>:<group>:<handler>"] trusted_hash` in `config.toml`
// (`codex-rs/hooks/src/engine/discovery.rs`: `hook_hash`, `hook_trust_status`). The TUI's startup
// hooks review writes that entry; `codex exec --dangerously-bypass-hook-trust` skips the check;
// nothing else runs an untrusted or modified handler, silently. The hash is sha256 over the
// key-sorted compact JSON of the normalized identity: the event's snake_case label, the group's
// matcher, and the one handler with Codex's defaults applied (`timeout` 600 when unset, `async`
// false, `statusMessage`/`additionalContextLimit` only when set and not the default). Checked
// against Codex 0.162.1's `hooks/list` for cv's six entries in every scope.

/// What Codex will do with one of cv's entries: run it, or skip it until reviewed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Trust {
    Trusted,
    Untrusted,
    /// Trusted once, then the entry changed (a moved binary, an edited matcher): skipped again.
    Modified,
}

impl Trust {
    fn as_str(self) -> &'static str {
        match self {
            Trust::Trusted => "trusted",
            Trust::Untrusted => "untrusted",
            Trust::Modified => "modified",
        }
    }
}

/// One of cv's entries as Codex identifies it: its state key, its current hash, and its trust.
struct TrustRow {
    name: HookName,
    key: String,
    hash: String,
    trust: Trust,
}

/// Codex's state key label for a hooks.json event (`hook_event_key_label`).
fn codex_event_label(event: &str) -> &'static str {
    match event {
        "PreToolUse" => "pre_tool_use",
        "PostToolUse" => "post_tool_use",
        "SessionStart" => "session_start",
        "UserPromptSubmit" => "user_prompt_submit",
        "SubagentStart" => "subagent_start",
        "SubagentStop" => "subagent_stop",
        "Stop" => "stop",
        _ => "unknown",
    }
}

/// The hash Codex computes for a `type: command` handler under `event` with `matcher`.
fn codex_hook_hash(event: &str, matcher: Option<&str>, handler: &Value) -> String {
    let mut hook = Map::new();
    hook.insert("type".into(), json!("command"));
    hook.insert("command".into(), handler.get("command").cloned().unwrap_or(Value::Null));
    hook.insert(
        "timeout".into(),
        json!(handler.get("timeout").and_then(Value::as_u64).unwrap_or(600).max(1)),
    );
    hook.insert(
        "async".into(),
        json!(handler.get("async").and_then(Value::as_bool).unwrap_or(false)),
    );
    if let Some(s) = handler.get("statusMessage").filter(|v| v.is_string()) {
        hook.insert("statusMessage".into(), s.clone());
    }
    if let Some(limit) = handler
        .get("additionalContextLimit")
        .and_then(Value::as_u64)
        .filter(|l| *l != 2500)
    {
        hook.insert("additionalContextLimit".into(), json!(limit));
    }
    let mut identity = Map::new();
    identity.insert("event_name".into(), json!(codex_event_label(event)));
    if let Some(m) = matcher {
        identity.insert("matcher".into(), json!(m));
    }
    identity.insert("hooks".into(), json!([Value::Object(hook)]));
    let mut identity = Value::Object(identity);
    identity.sort_all_objects();
    let mut digest = cv_core::digest::Sha256::new();
    digest.update(&serde_json::to_vec(&identity).unwrap_or_default());
    format!("sha256:{}", digest.finish_hex())
}

/// `[hooks.state]` of Codex's user `config.toml`: state key → `trusted_hash`. The TUI records
/// trust there for project hooks too.
fn codex_trust_store() -> HashMap<String, String> {
    let Some(text) = hosts()
        .find(|h| h.trust)
        .and_then(|h| h.user_dir())
        .and_then(|h| std::fs::read_to_string(h.join("config.toml")).ok())
    else {
        return HashMap::new();
    };
    let Ok(config) = text.parse::<toml::Table>() else {
        return HashMap::new();
    };
    config
        .get("hooks")
        .and_then(|h| h.get("state"))
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
        .filter_map(|(key, state)| {
            let hash = state.get("trusted_hash")?.as_str()?;
            Some((key.clone(), hash.to_string()))
        })
        .collect()
}

/// cv's entries in `settings` (a Codex hooks.json at `path`), each with the trust Codex gives it.
fn codex_trust(path: &Path, settings: &Value) -> Vec<TrustRow> {
    let store = codex_trust_store();
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return Vec::new();
    };
    // Codex keys by the config folder's canonical path (`/var/…` is `/private/var/…` on macOS).
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut rows = Vec::new();
    for (event, groups) in hooks {
        for (gi, group) in groups.as_array().into_iter().flatten().enumerate() {
            let matcher = group.get("matcher").and_then(Value::as_str);
            for (hi, handler) in group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let Some(name) = ours(handler) else { continue };
                let key = format!("{}:{}:{gi}:{hi}", path.display(), codex_event_label(event));
                let hash = codex_hook_hash(event, matcher, handler);
                let trust = match store.get(&key) {
                    None => Trust::Untrusted,
                    Some(h) if *h == hash => Trust::Trusted,
                    Some(_) => Trust::Modified,
                };
                rows.push(TrustRow { name, key, hash, trust });
            }
        }
    }
    rows
}

/// One phrase for a scope's trust: `trusted`, or the counts and what to do about them.
fn trust_summary(rows: &[TrustRow], scope: Scope) -> String {
    let count = |t: Trust| rows.iter().filter(|r| r.trust == t).count();
    let (untrusted, modified) = (count(Trust::Untrusted), count(Trust::Modified));
    if untrusted + modified == 0 {
        return "trusted".to_string();
    }
    let mut parts = Vec::new();
    if untrusted > 0 {
        parts.push(format!("{untrusted} untrusted"));
    }
    if modified > 0 {
        parts.push(format!("{modified} modified since trusted"));
    }
    let project = match scope {
        Scope::Project => " and this directory must be a trusted project",
        Scope::User => "",
    };
    format!(
        "{} — Codex skips them until you start `codex` and accept them in its startup hooks review{project} \
         (automation: `codex exec --dangerously-bypass-hook-trust`)",
        parts.join(", ")
    )
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

/// The `command` string for one hook: this binary, absolute, then `hook <name> --harness <h>`.
fn command_for(harness: Harness, exe: &str, name: HookName) -> String {
    format!("{} hook {} --harness {harness}", shell_quote(exe), name.as_str())
}

/// Is this handler one of ours? Recognized by shape: `<something>/cv hook <name>`, optionally
/// `--harness <h>` (quotes allowed around the program), so a reinstall from another binary path
/// replaces rather than duplicates.
fn ours(handler: &Value) -> Option<HookName> {
    let cmd = handler.get("command")?.as_str()?.trim();
    let (prog, rest) = cmd.rsplit_once(" hook ")?;
    let mut words = rest.split_whitespace();
    let name = HookName::parse(words.next()?)?;
    match (words.next(), words.next(), words.next()) {
        (None, ..) | (Some("--harness"), Some(_), None) => {}
        _ => return None,
    }
    let prog = prog.trim().trim_matches(|c| c == '\'' || c == '"');
    let base = Path::new(prog).file_name()?.to_str()?;
    (base == "cv" || base == "cv.exe").then_some(name)
}

fn handler_for(harness: Harness, exe: &str, spec: &Spec) -> Value {
    let mut h = Map::new();
    h.insert("type".into(), json!("command"));
    h.insert("command".into(), json!(command_for(harness, exe, spec.name)));
    if spec.asynchronous {
        h.insert("async".into(), json!(true));
    } else {
        h.insert("timeout".into(), json!(HOOK_TIMEOUT_SECS));
    }
    Value::Object(h)
}

/// Strip every handler of ours from `settings.hooks`, dropping groups and events left empty.
fn strip_ours(settings: &mut Value) {
    strip_where(settings, |_| true);
}

/// Strip our handlers whose hook `selected` picks, dropping groups and events left empty.
fn strip_where(settings: &mut Value, selected: impl Fn(HookName) -> bool) {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for ev in SPECS
        .iter()
        .chain(RECEIPTS.iter())
        .map(|s| s.event)
        .collect::<std::collections::BTreeSet<_>>()
    {
        if let Some(groups) = hooks.get_mut(ev).and_then(Value::as_array_mut) {
            for g in groups.iter_mut() {
                if let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                    hs.retain(|h| !ours(h).is_some_and(&selected));
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
fn with_ours(harness: Harness, mut settings: Value, exe: &str) -> Value {
    // The opt-in retired-agent guard and receipt log are not of the six; a reinstall keeps them.
    strip_where(&mut settings, |n| n != HookName::Retired && n != HookName::Receipt);
    let obj = settings.as_object_mut().expect("settings is an object");
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().expect("object");
    for spec in specs(harness) {
        let mut group = Map::new();
        if let Some(m) = spec.matcher {
            group.insert("matcher".into(), json!(m));
        }
        group.insert("hooks".into(), json!([handler_for(harness, exe, spec)]));
        let groups = hooks.entry(spec.event).or_insert_with(|| json!([]));
        if !groups.is_array() {
            *groups = json!([]);
        }
        groups.as_array_mut().expect("array").push(Value::Object(group));
    }
    settings
}

/// `settings` with the retired-agent guard present once, and nothing else of ours touched.
fn with_retired(harness: Harness, mut settings: Value, exe: &str) -> Value {
    strip_where(&mut settings, |n| n == HookName::Retired);
    let obj = settings.as_object_mut().expect("settings is an object");
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let groups = hooks
        .as_object_mut()
        .expect("object")
        .entry(RETIRED.event)
        .or_insert_with(|| json!([]));
    if !groups.is_array() {
        *groups = json!([]);
    }
    groups
        .as_array_mut()
        .expect("array")
        .push(json!({"hooks": [handler_for(harness, exe, &RETIRED)]}));
    settings
}

/// `settings` with the receipt log's two entries present once, and nothing else of ours touched.
fn with_receipts(harness: Harness, mut settings: Value, exe: &str) -> Value {
    strip_where(&mut settings, |n| n == HookName::Receipt);
    let obj = settings.as_object_mut().expect("settings is an object");
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().expect("object");
    for spec in &RECEIPTS {
        let groups = hooks.entry(spec.event).or_insert_with(|| json!([]));
        if !groups.is_array() {
            *groups = json!([]);
        }
        groups
            .as_array_mut()
            .expect("array")
            .push(json!({"hooks": [handler_for(harness, exe, spec)]}));
    }
    settings
}

fn opt_in_specs(harness: Harness) -> Vec<(&'static Spec, &'static str)> {
    let mut out = Vec::new();
    if host(harness).is_some_and(|h| h.retired_guard) {
        out.push((&RETIRED, "--retired"));
    }
    if host(harness).is_some_and(|h| h.receipts) {
        for s in &RECEIPTS {
            out.push((s, "--receipts"));
        }
    }
    out
}

/// Which of the given `specs` are present in `settings`, by `(event, matcher, name)`, with the command found.
fn present_specs<'a>(
    specs_iter: impl Iterator<Item = &'a Spec>,
    settings: &Value,
) -> HashMap<(&'static str, Option<&'static str>, HookName), String> {
    let mut found = HashMap::new();
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return found;
    };
    for spec in specs_iter {
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

/// Which of our standard six entries are present in `settings`, by `(event, matcher, name)`.
fn present(harness: Harness, settings: &Value) -> HashMap<(&'static str, Option<&'static str>, HookName), String> {
    present_specs(specs(harness), settings)
}

/// Count every handler of ours in `settings.hooks` (standard six, opt-in `retired`/`receipt`, and
/// any `PostToolUse` `edit`/`receipt` entries).
fn total_ours_entries(settings: &Value) -> usize {
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return 0;
    };
    hooks
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|g| g.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter(|h| ours(h).is_some())
        .count()
}

fn hook_harness(harness: &str) -> Result<Harness> {
    match Harness::parse(harness).filter(|h| host(*h).is_some()) {
        Some(h) => Ok(h),
        None => bail!("`cv hooks` supports verified Claude Code (`claude`) and Codex (`codex`) hooks; {harness:?} has no verified hook contract"),
    }
}

fn this_exe() -> Result<String> {
    let exe = std::env::current_exe().context("locating the cv binary")?;
    Ok(exe.to_string_lossy().into_owned())
}

pub(crate) fn cmd_hooks(cmd: HooksCmd) -> Result<()> {
    match cmd {
        HooksCmd::Install {
            harness,
            scope,
            receipts: true,
            ..
        } => {
            let harness = hook_harness(&harness)?;
            if !host(harness).is_some_and(|h| h.receipts) {
                bail!("the receipt log reads Claude Code's `tool_use_id`; {harness} is not wired");
            }
            let (settings_path, _) = paths(harness, scope)?;
            let before = read_settings(&settings_path)?;
            let after = with_receipts(harness, before.clone(), &this_exe()?);
            if after == before {
                println!(
                    "cv hooks: the receipt log is already in {}; nothing written",
                    home_rel(&settings_path)
                );
            } else {
                write_settings(&settings_path, &after)?;
                println!(
                    "cv hooks: installed the receipt log (PreToolUse + PostToolUse, every tool) → {}",
                    home_rel(&settings_path)
                );
                println!(
                    "  (chained receipts land in <cv home>/state.db; `cv hooks remove --receipts` deletes the entries)"
                );
            }
            Ok(())
        }
        HooksCmd::Remove {
            harness,
            scope,
            receipts: true,
            ..
        } => {
            let harness = hook_harness(&harness)?;
            let (settings_path, _) = paths(harness, scope)?;
            let before = read_settings(&settings_path)?;
            let mut after = before.clone();
            strip_where(&mut after, |n| n == HookName::Receipt);
            if after == before {
                println!(
                    "cv hooks: no receipt log entries in {}; nothing written",
                    home_rel(&settings_path)
                );
            } else {
                write_settings(&settings_path, &after)?;
                println!(
                    "cv hooks: removed the receipt log entries from {}",
                    home_rel(&settings_path)
                );
            }
            Ok(())
        }
        HooksCmd::Install {
            harness,
            scope,
            retired: true,
            receipts: false,
        } => {
            let harness = hook_harness(&harness)?;
            if !host(harness).is_some_and(|h| h.retired_guard) {
                bail!("the retired-agent guard reads Claude Code's sub-agent `agent_id`; {harness} has no such field");
            }
            let (settings_path, _) = paths(harness, scope)?;
            let before = read_settings(&settings_path)?;
            let after = with_retired(harness, before.clone(), &this_exe()?);
            if after == before {
                println!(
                    "cv hooks: the retired-agent guard is already in {}; nothing written",
                    home_rel(&settings_path)
                );
            } else {
                write_settings(&settings_path, &after)?;
                println!(
                    "cv hooks: installed the retired-agent guard (PreToolUse, every tool) → {}",
                    home_rel(&settings_path)
                );
                println!("  (`cv retire --list` shows who it refuses; `cv hooks remove --retired` deletes it)");
            }
            Ok(())
        }
        HooksCmd::Remove {
            harness,
            scope,
            retired: true,
            receipts: false,
        } => {
            let harness = hook_harness(&harness)?;
            let (settings_path, _) = paths(harness, scope)?;
            let before = read_settings(&settings_path)?;
            let mut after = before.clone();
            strip_where(&mut after, |n| n == HookName::Retired);
            if after == before {
                println!(
                    "cv hooks: no retired-agent guard in {}; nothing written",
                    home_rel(&settings_path)
                );
            } else {
                write_settings(&settings_path, &after)?;
                println!(
                    "cv hooks: removed the retired-agent guard from {}",
                    home_rel(&settings_path)
                );
            }
            Ok(())
        }
        HooksCmd::Install {
            harness,
            scope,
            retired: false,
            receipts: false,
        } => {
            let harness = hook_harness(&harness)?;
            let (settings_path, skill_path) = paths(harness, scope)?;
            let exe = this_exe()?;
            let before = read_settings(&settings_path)?;
            let after = with_ours(harness, before.clone(), &exe);
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
                println!("  (takes effect in new harness sessions; `cv hooks show` lists the entries, `cv hooks remove` deletes them)");
            }
            if needs_trust(harness) {
                println!(
                    "  Codex trust: {}",
                    trust_summary(&codex_trust(&settings_path, &after), scope)
                );
            }
            Ok(())
        }
        HooksCmd::Remove {
            harness,
            scope,
            retired: false,
            receipts: false,
        } => {
            let harness = hook_harness(&harness)?;
            let (settings_path, skill_path) = paths(harness, scope)?;
            let before = read_settings(&settings_path)?;
            let n = total_ours_entries(&before);
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
            let harness = hook_harness(&harness)?;
            let (settings_path, skill_path) = paths(harness, scope)?;
            let exe = this_exe()?;
            let before = read_settings(&settings_path)?;
            let after = with_ours(harness, before.clone(), &exe);
            let have = present(harness, &before);
            let opt_specs = opt_in_specs(harness);
            let have_opt = present_specs(opt_specs.iter().map(|(s, _)| *s), &before);
            let skill_present = std::fs::read_to_string(&skill_path).ok().as_deref() == Some(SKILL);
            // Codex: the trust each present entry has today (the file as it is, not as install would leave it).
            let trust = match needs_trust(harness) {
                true => codex_trust(&settings_path, &before),
                false => Vec::new(),
            };
            let trust_of = |s: &Spec| {
                trust
                    .iter()
                    .find(|r| r.name == s.name && r.key.contains(&format!(":{}:", codex_event_label(s.event))))
            };
            let all_hook_names = [
                (HookName::Compact, false),
                (HookName::Start, false),
                (HookName::Lane, false),
                (HookName::Decisions, false),
                (HookName::Edit, false),
                (HookName::Retired, true),
                (HookName::Receipt, true),
            ];
            let hook_summary: Vec<Value> = all_hook_names
                .iter()
                .map(|&(hname, is_opt)| {
                    let want = command_for(harness, &exe, hname);
                    let relevant: Vec<&Spec> = if is_opt {
                        opt_specs
                            .iter()
                            .filter(|(s, _)| s.name == hname)
                            .map(|(s, _)| *s)
                            .collect()
                    } else {
                        specs(harness).filter(|s| s.name == hname).collect()
                    };
                    let map = if is_opt { &have_opt } else { &have };
                    let is_installed = !relevant.is_empty()
                        && relevant
                            .iter()
                            .all(|s| map.get(&(s.event, s.matcher, s.name)) == Some(&want));
                    json!({
                        "hook": hname.as_str(),
                        "opt_in": is_opt,
                        "installed": is_installed,
                    })
                })
                .collect();
            if json {
                let (mut installed, mut missing, mut opt_in) = (Vec::new(), Vec::new(), Vec::new());
                for s in specs(harness) {
                    let want = command_for(harness, &exe, s.name);
                    let mut row = json!({"event": s.event, "matcher": s.matcher, "hook": s.name.as_str(),
                        "command": want});
                    if let Some(t) = trust_of(s) {
                        row["trust"] = json!(t.trust.as_str());
                        row["trust_key"] = json!(t.key);
                        row["current_hash"] = json!(t.hash);
                    }
                    match have.get(&(s.event, s.matcher, s.name)) {
                        Some(cmd) if *cmd == command_for(harness, &exe, s.name) => installed.push(row),
                        _ => missing.push(row),
                    }
                }
                for &(s, flag) in &opt_specs {
                    let want = command_for(harness, &exe, s.name);
                    let is_inst = have_opt.get(&(s.event, s.matcher, s.name)) == Some(&want);
                    let mut row = json!({
                        "event": s.event,
                        "matcher": s.matcher,
                        "hook": s.name.as_str(),
                        "opt_in": true,
                        "flag": flag,
                        "installed": is_inst,
                        "command": want,
                    });
                    if let Some(t) = trust_of(s) {
                        row["trust"] = json!(t.trust.as_str());
                        row["trust_key"] = json!(t.key);
                        row["current_hash"] = json!(t.hash);
                    }
                    if is_inst {
                        installed.push(row.clone());
                    }
                    opt_in.push(row);
                }
                let mut out = json!({
                    "settings_path": settings_path,
                    "installed": installed,
                    "missing": missing,
                    "opt_in": opt_in,
                    "hooks": hook_summary,
                    "skill_path": skill_path,
                    "skill_present": skill_present,
                    "before": before,
                    "after": after,
                    "would_change": after != before || !skill_present,
                });
                if needs_trust(harness) {
                    out["trust"] = json!(trust_summary(&trust, scope));
                }
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }
            println!("# cv hooks — {} ({:?} scope)\n", home_rel(&settings_path), scope);
            for s in specs(harness) {
                let want = command_for(harness, &exe, s.name);
                let mark = match have.get(&(s.event, s.matcher, s.name)) {
                    Some(cmd) if *cmd == want => "✓",
                    Some(_) => "~", // ours, another binary path: install replaces it
                    None => "+",
                };
                let matcher = s.matcher.map(|m| format!(" [{m}]")).unwrap_or_default();
                let trust = trust_of(s)
                    .map(|t| format!(" · {}", t.trust.as_str()))
                    .unwrap_or_default();
                println!("  {mark} {:<17}{matcher:<18} {want}{trust}", s.event);
            }
            for &(s, flag) in &opt_specs {
                let want = command_for(harness, &exe, s.name);
                let (mark, suffix) = match have_opt.get(&(s.event, s.matcher, s.name)) {
                    Some(cmd) if *cmd == want => ("✓", format!(" (opt-in {flag})")),
                    Some(_) => ("~", format!(" (opt-in {flag})")),
                    None => ("-", format!(" (opt-in {flag})")),
                };
                let matcher = s.matcher.map(|m| format!(" [{m}]")).unwrap_or_default();
                let trust = trust_of(s)
                    .map(|t| format!(" · {}", t.trust.as_str()))
                    .unwrap_or_default();
                println!("  {mark} {:<17}{matcher:<18} {want}{suffix}{trust}", s.event);
            }
            println!(
                "  {} skill {}",
                if skill_present { "✓" } else { "+" },
                home_rel(&skill_path)
            );
            println!(
                "\n{}",
                if after == before && skill_present {
                    format!("installed; `cv hooks install {harness}` would write nothing")
                } else {
                    format!("✓ present · + `cv hooks install {harness}` adds · ~ replaces · - opt-in not installed")
                }
            );
            if needs_trust(harness) && !trust.is_empty() {
                println!("Codex trust: {}", trust_summary(&trust, scope));
            }
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

/// One scope's settings file as `cv doctor` reads it: how many of cv's entries it holds, or why
/// it cannot be read (a file that is not JSON is not "not installed"), and Codex's trust in them.
pub(crate) struct ScopeHooks {
    pub scope: Scope,
    pub path: PathBuf,
    pub entries: std::result::Result<usize, String>,
    trust: Vec<TrustRow>,
}

impl ScopeHooks {
    /// Every scope of `harness` whose settings path resolves, with its parsed settings when they read.
    fn read(harness: Harness) -> Vec<(ScopeHooks, Option<Value>)> {
        [Scope::User, Scope::Project]
            .into_iter()
            .filter_map(|scope| {
                let (path, _) = paths(harness, scope).ok()?;
                let (entries, trust, settings) = match read_settings(&path) {
                    Ok(settings) => {
                        let trust = match needs_trust(harness) {
                            true => codex_trust(&path, &settings),
                            false => Vec::new(),
                        };
                        (Ok(present(harness, &settings).len()), trust, Some(settings))
                    }
                    Err(e) => (Err(e.root_cause().to_string()), Vec::new(), None),
                };
                Some((
                    ScopeHooks {
                        scope,
                        path,
                        entries,
                        trust,
                    },
                    settings,
                ))
            })
            .collect()
    }

    /// `6/6 in <path> (User)`, or `<path> (User) unreadable (<why>)`.
    fn line(&self) -> String {
        let (path, scope) = (home_rel(&self.path), self.scope);
        match &self.entries {
            Ok(n) => format!("{n}/{} in {path} ({scope:?})", SPECS.len()),
            Err(why) => format!("{path} ({scope:?}) unreadable ({why})"),
        }
    }

    /// Worth a doctor row: it holds entries of ours, or cannot be read.
    fn reportable(&self) -> bool {
        self.entries.as_ref().map_or(true, |n| *n > 0)
    }

    fn json(&self) -> Value {
        let mut row = json!({
            "scope": format!("{:?}", self.scope).to_lowercase(), "entries": self.entries.as_ref().ok(),
            "of": SPECS.len(), "settings_path": self.path,
        });
        if let Err(why) = &self.entries {
            row["error"] = json!(why);
        }
        row
    }
}

/// The policy host's hooks per scope (Claude's), and the policy in force — what `cv doctor` prints.
pub(crate) fn status() -> (Vec<ScopeHooks>, Policy) {
    let mut scopes = Vec::new();
    let mut auto = None;
    let mut idle = None;
    let mut env_block: HashMap<String, String> = HashMap::new();
    let policy_hosts = hosts().filter(|h| h.policy);
    for (scope, settings) in policy_hosts.flat_map(|h| ScopeHooks::read(h.harness)) {
        scopes.push(scope);
        let Some(settings) = settings else { continue };
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

/// The trust hosts' hooks per scope (Codex's), the scopes worth a row only.
fn codex_status() -> Vec<ScopeHooks> {
    hosts()
        .filter(|h| h.trust)
        .flat_map(|h| ScopeHooks::read(h.harness))
        .map(|(scope, _)| scope)
        .filter(ScopeHooks::reportable)
        .collect()
}

/// The three lines `cv doctor` prints under its header.
pub(crate) fn doctor_lines() -> Vec<String> {
    let (scopes, policy) = status();
    let installed: Vec<String> = scopes.iter().filter(|s| s.reportable()).map(ScopeHooks::line).collect();
    let hooks = if installed.is_empty() {
        "not installed — `cv hooks install claude` wires compaction, session start, lanes, decisions and the edit guard"
            .to_string()
    } else {
        installed.join(", ")
    };
    let codex: Vec<String> = codex_status()
        .iter()
        .map(|s| match s.entries {
            Ok(_) => format!("{}, {}", s.line(), trust_summary(&s.trust, s.scope)),
            Err(_) => s.line(),
        })
        .collect();
    let codex = if codex.is_empty() {
        "not installed — `cv hooks install codex` wires the same five loops for Codex".to_string()
    } else {
        codex.join("; ")
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
        format!("Codex hooks: {codex}"),
        format!("Policy:      {}", knobs.join(" · ")),
    ]
}

pub(crate) fn doctor_json() -> Value {
    let (scopes, policy) = status();
    json!({
        "installed": scopes.iter().map(ScopeHooks::json).collect::<Vec<_>>(),
        "codex": codex_status().iter().map(|s| {
            let mut row = s.json();
            row["trust"] = s.trust.iter().map(|r| json!({"hook": r.name.as_str(), "key": r.key, "trust": r.trust.as_str()})).collect();
            row
        }).collect::<Vec<_>>(),
        "policy": {
            "auto_compact_enabled": policy.auto_compact_enabled,
            "idle_compaction": policy.idle_compaction,
            "env": policy.env.iter().map(|(k, v)| (k.to_string(), json!(v))).collect::<Map<String, Value>>(),
        }
    })
}

// ───────────────────────────── the hook bodies ─────────────────────────────

/// The harness a `cv hook` call speaks for: `--harness`, else Claude (the default every
/// `cv hooks` subcommand names).
fn hook_call_harness(flag: Option<&str>) -> Result<Harness> {
    if cv_core::env::var_os("CV_HOOK_HARNESS").is_some() {
        return crate::util::usage(
            "`$CV_HOOK_HARNESS` was removed in 0.18.0 — `cv hooks install codex` writes `--harness codex`",
        );
    }
    hook_harness(flag.unwrap_or("claude"))
}

/// `cv hook <name>`: read stdin, run the body, print its one-line JSON (or nothing). A body that
/// fails prints its error on stderr and still exits 0 — a hook must never block the harness. A
/// terminal on stdin means no harness is calling: usage, exit 2, instead of waiting for JSON.
pub(crate) fn cmd_hook(name: HookName, harness: Option<String>) -> Result<()> {
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return crate::util::usage(format!(
            "cv hook {} reads the harness's hook JSON on stdin, and stdin is a terminal; \
             `cv hooks install <claude|codex>` writes the settings entries that pipe it",
            name.as_str()
        ));
    }
    let harness = hook_call_harness(harness.as_deref())?;
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let payload: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "cv hook {}: stdin is not the hook JSON ({e}); nothing to do",
                name.as_str()
            );
            return Ok(());
        }
    };
    let decoded = cv_core::harness::rootless(harness)
        .context("no adapter")
        .and_then(|adapter| adapter.hook_event(&payload));
    let out = match decoded {
        Ok(None) => Ok(None),
        Ok(Some(event)) => match name {
            HookName::Compact => hook_compact(&event),
            HookName::Start => hook_start(&event),
            HookName::Lane => hook_lane(&event, &payload),
            HookName::Decisions => hook_decisions(&event),
            HookName::Edit => hook_edit(&event),
            HookName::Retired => hook_retired(&event, &payload),
            HookName::Receipt => hook_receipt(&event, &payload),
        },
        Err(e) => Err(e),
    };
    match out {
        Ok(Some(v)) => println!("{v}"),
        Ok(None) => {}
        Err(e) => eprintln!("cv hook {}: {e:#}", name.as_str()),
    }
    Ok(())
}

/// How a hook names a session to the `cv` commands it points at: a Claude id short, a Codex id
/// whole (its time-ordered prefix is shared), any other harness-qualified.
fn selector(harness: Harness, id: &str) -> String {
    match harness {
        Harness::Claude => short_id(id),
        Harness::Codex => format!("codex:{id}"),
        Harness::Grok
        | Harness::OpenCode
        | Harness::Gemini
        | Harness::Antigravity
        | Harness::Hermes
        | Harness::OpenClaw
        | Harness::Cursor
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Qwen
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => format!("{harness}:{}", short_id(id)),
    }
}

fn adapter_of(ev: &Event) -> Result<&'static dyn cv_core::harness::Adapter> {
    cv_core::harness::rootless(ev.harness).with_context(|| format!("no {} adapter", ev.harness))
}

fn context(event: &str, text: String) -> Value {
    json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}})
}

fn is_compressed_store(path: &Path) -> bool {
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "zst" | "gz" | "xz" | "bz2" | "lz4"))
    {
        return true;
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic).unwrap_or(0);
    // Zstandard frame (0xFD2FB528), Zstandard skippable frame (0x184D2A50..=0x184D2A5F), or gzip (0x1F 0x8B).
    (n >= 4 && (magic == [0x28, 0xB5, 0x2F, 0xFD] || ((magic[0] & 0xF0) == 0x50 && magic[1..4] == [0x2A, 0x4D, 0x18])))
        || (n >= 2 && magic[..2] == [0x1F, 0x8B])
}

fn is_binary_or_compressed_store(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return true;
    }
    if path.extension().and_then(|e| e.to_str()).is_some_and(|ext| {
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "db" | "sqlite" | "sqlite3" | "vscdb" | "zst" | "gz" | "xz" | "bz2" | "lz4"
        )
    }) {
        return true;
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 16];
    let n = f.read(&mut magic).unwrap_or(0);
    (n >= 16 && &magic == b"SQLite format 3\0")
        || (n >= 4
            && (magic[..4] == [0x28, 0xB5, 0x2F, 0xFD]
                || ((magic[0] & 0xF0) == 0x50 && magic[1..4] == [0x2A, 0x4D, 0x18])))
        || (n >= 2 && magic[..2] == [0x1F, 0x8B])
}

/// Read the last `bytes` of a plain-text transcript `path` as raw bytes without allocating a
/// `String`. Binary SQLite databases and compressed archives return `None` so callers never seek
/// raw mid-file binary/compressed bytes.
fn tail_bytes(path: &Path, bytes: u64) -> Option<Vec<u8>> {
    use std::io::{Seek, SeekFrom};
    if is_binary_or_compressed_store(path) {
        return None;
    }
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
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
/// it discarded. A transcript up to [`HOOK_READ_BYTES`] is streamed the way `cv show --kind
/// compaction_boundary` does it (exact message index and ordinal); a larger one is read from its
/// tail (trigger and size exact, the index left to that view, and the text says how much it
/// read), so the hook stays inside its budget however long the session has run.
fn hook_compact(ev: &Event) -> Result<Option<Value>> {
    let Some(path) = ev.transcript() else {
        return Ok(None);
    };
    let adapter = adapter_of(ev)?;
    let id = selector(ev.harness, &ev.session.id);
    // `cv rewind` writes a derived copy: only a harness with derived copies is pointed at it.
    let rewinds = adapter.identity().is_some();
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > HOOK_READ_BYTES {
        // A tail without a boundary (one far back in a long transcript) still gets the pointers.
        let read = format!(
            "the hook read the last {} of this {} transcript",
            cv_core::ir::human_bytes(HOOK_READ_BYTES),
            cv_core::ir::human_bytes(size)
        );
        let what = match adapter.last_compact_boundary(path, HOOK_READ_BYTES)? {
            Some((trigger, pre_tokens)) => format!(
                " ({}, {}; {read}, so the index is not given)",
                trigger.as_deref().unwrap_or("compaction"),
                pre_tokens
                    .map(|t| format!("{} tokens", fmt_int(t)))
                    .unwrap_or_else(|| "size unrecorded".into())
            ),
            None => format!(" (no boundary recorded in what {read})"),
        };
        return Ok(Some(context(
            "SessionStart",
            format!(
                "context compacted{what}. `cv show {id} --kind compaction_boundary,compaction_summary` lists this session's \
                 boundaries with their message indexes; `cv show {id} --pre-compaction N` reads what the Nth discarded (`cv show \
                 {id} --kind prompt,answer --pre-compaction N` just its prompts); {rewind} \
                 {AFFORDANCE_POINTER}",
                rewind = if rewinds { format!("`cv rewind {id} --at <msg>` resumes the agent as of a message.") } else { String::new() },
                AFFORDANCE_POINTER = affordance_pointer(&id)
            ),
        )));
    }
    let comps = cv_core::compaction::detect(&ev.session, false)?;
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
    if idx > 0 && rewinds {
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

fn commit_re() -> &'static regex::bytes::Regex {
    static RE: std::sync::OnceLock<regex::bytes::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::bytes::Regex::new(r#"\[([^\]\s\\"]+)(?: \([^)]*\))? ([0-9a-f]{7,40})\] "#).expect("static regex")
    })
}

/// What the start pack says of a session's last commit: the short sha of the last
/// `[branch sha] subject` (a `git commit` result) in the last `bytes` of its transcript — never
/// a parse of the whole file — or, when none is there and the transcript is longer, how far back
/// it looked.
fn last_commit(path: &Path, bytes: u64) -> String {
    let re = commit_re();
    let Some(buf) = tail_bytes(path, bytes) else {
        return String::new();
    };
    match re.captures_iter(&buf).last() {
        Some(c) => {
            let sha = std::str::from_utf8(&c[2]).unwrap_or("");
            format!(" · last commit {}", &sha[..7.min(sha.len())])
        }
        None if std::fs::metadata(path).is_ok_and(|m| m.len() > bytes) => {
            format!(" · no commit in its last {}", cv_core::ir::human_bytes(bytes))
        }
        None => String::new(),
    }
}

/// Session-aware commit lookup for `hook_start`: plain-text logs tail raw bytes directly; binary
/// SQLite stores and compressed rollouts narrow to the indexed `events` commit position (or the
/// session's tail messages) and scan `ToolResult` chunks as raw bytes without allocating per-result
/// `String`s or giving up at 2,000 messages.
fn last_commit_session(r: &SessionRef, bytes: u64) -> String {
    if !is_binary_or_compressed_store(&r.path) {
        return last_commit(&r.path, bytes);
    }
    if bytes == 0 {
        return String::new();
    }
    if is_compressed_store(&r.path) && std::fs::metadata(&r.path).is_ok_and(|m| m.len() > bytes) {
        return String::new();
    }
    let Some(adapter) = cv_core::harness::rootless(r.harness) else {
        return String::new();
    };
    // Consult indexed events when already cataloged: start scanning at the earliest of the
    // recent `git` command events (or the last `TAIL_MESSAGES` messages, whichever comes first),
    // so long sessions (>2,000 messages) find their commits without resolving old prefix tool results.
    const TAIL_MESSAGES: usize = 256;
    let indexed_git_msg = cv_core::events::events_for(r.harness.as_str(), &r.id, Some("command"))
        .ok()
        .and_then(|evs| {
            evs.iter()
                .rev()
                .filter(|e| e.target.as_deref().is_some_and(|t| t == "git" || t.ends_with("/git")))
                .take(16)
                .last()
                .map(|e| e.msg_idx.max(0) as u64)
        });
    let tail_start = r.message_count.map(|n| n.saturating_sub(TAIL_MESSAGES) as u64);
    let min_msg_idx = match (indexed_git_msg, tail_start) {
        (Some(g), Some(t)) => g.min(t),
        (Some(g), None) => g,
        (None, Some(t)) => t,
        (None, None) => 0,
    };

    struct CommitSink {
        resolver: cv_core::Resolver,
        min_msg_idx: u64,
        seen_msgs: u64,
        remaining: u64,
        sha: Option<String>,
    }
    impl cv_core::MessageSink for CommitSink {
        fn message(&mut self, m: cv_core::Message) -> cv_core::Flow {
            let idx = self.seen_msgs;
            self.seen_msgs += 1;
            if idx < self.min_msg_idx {
                return cv_core::Flow::Continue;
            }
            let re = commit_re();
            for block in &m.content {
                let text = match block {
                    cv_core::Block::ToolResult { content, .. } => content,
                    _ => continue,
                };
                if self.remaining == 0 {
                    return cv_core::Flow::Stop;
                }
                let mut overlap: [u8; 96] = [0; 96];
                let mut overlap_len: usize = 0;
                let _ = text.try_for_each_chunk(&self.resolver, 64 * 1024, |chunk| {
                    if self.remaining == 0 {
                        return Ok(());
                    }
                    let raw = chunk.as_bytes();
                    let take = raw.len().min(self.remaining as usize);
                    let slice = &raw[..take];
                    self.remaining = self.remaining.saturating_sub(take as u64);
                    if overlap_len == 0 {
                        if let Some(c) = re.captures_iter(slice).last() {
                            if let Ok(s) = std::str::from_utf8(&c[2]) {
                                self.sha = Some(s[..7.min(s.len())].to_string());
                            }
                        }
                    } else {
                        let mut combined = Vec::with_capacity(overlap_len + slice.len());
                        combined.extend_from_slice(&overlap[..overlap_len]);
                        combined.extend_from_slice(slice);
                        if let Some(c) = re.captures_iter(&combined).last() {
                            if let Ok(s) = std::str::from_utf8(&c[2]) {
                                self.sha = Some(s[..7.min(s.len())].to_string());
                            }
                        }
                    }
                    let keep = slice.len().min(overlap.len());
                    overlap[..keep].copy_from_slice(&slice[slice.len() - keep..]);
                    overlap_len = keep;
                    Ok(())
                });
            }
            if self.remaining == 0 {
                cv_core::Flow::Stop
            } else {
                cv_core::Flow::Continue
            }
        }
    }
    let mut sink = CommitSink {
        resolver: cv_core::Resolver::new(Some(r.path.clone())),
        min_msg_idx,
        seen_msgs: 0,
        remaining: bytes,
        sha: None,
    };
    let _ = cv_core::stream::stream_counted(
        adapter,
        r,
        &cv_core::Demand::LEAN.with_spans(),
        &mut sink,
        cv_core::stream::Pass::default(),
    );
    sink.sha.map_or_else(String::new, |sha| format!(" · last commit {sha}"))
}

fn ago(t: Option<chrono::DateTime<chrono::Utc>>) -> String {
    t.map(|t| format!("{} ago", cv_core::task::age_short(t, chrono::Utc::now())))
        .unwrap_or_else(|| "undated".into())
}

/// `(who, n)` when the task log owes `n > 0` decisions to the identity `cv task inbox` would use.
/// The one policy for both hooks that mention them: an unreadable task log is said on stderr and
/// owes nothing, so it never costs the start pack its sessions.
fn owed_decisions(hook: HookName) -> Option<(String, usize)> {
    let who = cv_core::task::actor(None, "cv");
    // Every prompt asks: the stored fold and the events appended since, never the whole log.
    let outcome = match cv_core::task::fold() {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("cv hook {}: {e:#}", hook.as_str());
            return None;
        }
    };
    let n = cv_core::task::inbox(&outcome.model, &who)
        .iter()
        .filter(|e| matches!(e.reason, cv_core::task::InboxReason::DecisionOwed))
        .count();
    (n > 0).then_some((who, n))
}

/// `SessionStart` (`startup|resume`): this directory's last sessions from the catalog as it stands
/// (ids, ages, titles, last commit shas) and the decisions owed — capped, nothing else.
fn hook_start(ev: &Event) -> Result<Option<Value>> {
    let cwd = match ev.normalized_cwd() {
        Some(c) => c,
        None => cv_core::tools::classify::normalize_path(&std::env::current_dir()?, None),
    };
    let raw_cwd = ev.cwd.as_deref();
    let mut rows: Vec<SessionRef> = cv_core::cataloged_sessions()
        .into_iter()
        .filter(|r| r.id != ev.session.id && !r.id.starts_with("agent-"))
        .filter(|r| {
            r.cwd.as_deref().is_some_and(|c| {
                c == cwd || raw_cwd == Some(c) || cv_core::tools::classify::normalize_path(c, None) == cwd
            })
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.updated_at.or(r.created_at)));
    rows.truncate(START_SESSIONS);
    let owed = owed_decisions(HookName::Start);
    if rows.is_empty() && owed.is_none() {
        return Ok(None);
    }
    let mut lines = Vec::new();
    // The hooks' read budget, split across the transcripts listed.
    let tail = HOOK_READ_BYTES / rows.len().max(1) as u64;
    if !rows.is_empty() {
        lines.push(format!(
            "cv: {} earlier session(s) in this directory (catalog as of the last `cv ls`; newest first). \
             `cv show <id>` reads one.",
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
            let commit = last_commit_session(r, tail);
            let id = selector(r.harness, &r.id);
            lines.push(format!(
                "- {id} · {} · {} msgs · {title}{commit}",
                ago(r.updated_at.or(r.created_at)),
                r.message_count.map_or("–".to_string(), |n| n.to_string())
            ));
        }
    }
    if let Some((who, owed)) = owed {
        lines.push(format!(
            "- {owed} decision(s) owed to {who}: `cv task inbox` lists them."
        ));
    }
    // Every line is bounded (six rows, titles cut at 60 characters), well under the harness's
    // 10,000-character limit on hook context, so nothing is dropped.
    Ok(Some(context("SessionStart", lines.join("\n"))))
}

/// `SubagentStart` / `SubagentStop`: record the lane run in `state.db` (`lane_runs` +
/// `lane_endpoints`) and append one row to the lane ledger; no output, no model action.
fn hook_lane(ev: &Event, payload: &Value) -> Result<Option<Value>> {
    let event = match ev.event.as_deref() {
        Some("SubagentStart") => cv_core::lanes::LaneEventKind::Start,
        Some("SubagentStop") => cv_core::lanes::LaneEventKind::Stop,
        _ => return Ok(None),
    };
    let Some(agent) = ev.agent.as_ref() else {
        return Ok(None);
    };
    let now = chrono::Utc::now();
    let now_ms = now.timestamp_millis();
    let last_text = agent
        .last_message
        .as_deref()
        .and_then(|t| t.lines().find(|l| !l.trim().is_empty()))
        .map(|l| cv_core::ir::truncate(l.trim(), 200));
    let norm_cwd = ev.normalized_cwd();

    let resolved_cwd = norm_cwd.or_else(|| ev.cwd.clone());
    cv_core::lanes::append_lane_event(&LaneEvent {
        ts: now,
        event,
        session_id: ev.session.id.clone(),
        agent_id: agent.id.clone(),
        agent_type: agent.kind.clone(),
        transcript_path: ev.transcript().map(Path::to_path_buf),
        agent_transcript_path: agent.transcript.clone(),
        cwd: resolved_cwd.clone(),
        last_text: last_text.clone(),
    })
    .context("appending to the lane ledger")?;

    if !cv_core::env::frozen() {
        if let Ok(store) = cv_core::state::StateStore::default_store() {
            let bare_id = cv_core::retired::bare(&agent.id).to_string();
            let pid = payload.get("pid").and_then(Value::as_i64).or_else(|| {
                #[cfg(unix)]
                {
                    let ppid = unsafe { libc::getppid() } as i64;
                    (ppid > 1).then_some(ppid)
                }
                #[cfg(not(unix))]
                {
                    None
                }
            });
            let proc_start_time = pid.and_then(cv_core::state::tables::proc_start_time_of);
            let derived_agent_tp = agent.transcript.clone().or_else(|| {
                cv_core::harness::rootless(ev.harness).and_then(|a| a.agent_transcript(&ev.session, &bare_id, None))
            });
            let existing = store
                .list_lane_runs_for_session(&ev.session.id)
                .unwrap_or_default()
                .into_iter()
                .rev()
                .find(|r| r.agent_id.as_deref() == Some(bare_id.as_str()));

            let target_run_id = match event {
                cv_core::lanes::LaneEventKind::Start => {
                    let active_existing = existing.filter(|r| r.exited_at.is_none());
                    let rid = active_existing
                        .as_ref()
                        .map(|r| r.run_id.clone())
                        .unwrap_or_else(|| format!("{}:{bare_id}", ev.session.id));
                    let _ = store.record_lane_start(&cv_core::state::LaneRun {
                        run_id: rid.clone(),
                        session_id: ev.session.id.clone(),
                        agent_id: Some(bare_id),
                        agent_type: agent.kind.clone(),
                        harness: Some(ev.harness.as_str().to_string()),
                        pid,
                        proc_start_time,
                        boot_id: active_existing.and_then(|r| r.boot_id),
                        transcript_path: ev.transcript().map(|p| p.to_string_lossy().into_owned()),
                        agent_transcript_path: derived_agent_tp.map(|p| p.to_string_lossy().into_owned()),
                        cwd: resolved_cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                        started_at: now_ms,
                        exited_at: None,
                        exit_status: None,
                        last_text: None,
                    });
                    rid
                }
                cv_core::lanes::LaneEventKind::Stop => {
                    let rid = existing
                        .as_ref()
                        .map(|r| r.run_id.clone())
                        .unwrap_or_else(|| format!("{}:{bare_id}", ev.session.id));
                    let updated = store
                        .record_lane_exit(&rid, now_ms, Some("hook_stop"), last_text.as_deref())
                        .unwrap_or(false);
                    if !updated {
                        let _ = store.record_lane_start(&cv_core::state::LaneRun {
                            run_id: rid.clone(),
                            session_id: ev.session.id.clone(),
                            agent_id: Some(bare_id),
                            agent_type: agent.kind.clone(),
                            harness: Some(ev.harness.as_str().to_string()),
                            pid,
                            proc_start_time,
                            boot_id: None,
                            transcript_path: ev.transcript().map(|p| p.to_string_lossy().into_owned()),
                            agent_transcript_path: derived_agent_tp.map(|p| p.to_string_lossy().into_owned()),
                            cwd: resolved_cwd.as_ref().map(|p| p.to_string_lossy().into_owned()),
                            started_at: now_ms,
                            exited_at: Some(now_ms),
                            exit_status: Some("stop_only".into()),
                            last_text,
                        });
                    }
                    rid
                }
            };
            if let Ok(ep) = cv_core::env::var("CV_ENDPOINT") {
                let ep = ep.trim();
                if !ep.is_empty() {
                    let _ = store.bind_lane_endpoint(&target_run_id, ep);
                }
            }
        }
    }
    Ok(None)
}

/// Extract cross-harness target agent identifiers addressed by a tool call (via `tools::classify`),
/// covering Claude `SendMessage` (`to`), Codex `send_input` (`id`) and Code Mode `exec`, OpenCode /
/// Antigravity `send_message`, `Task`/`Agent` `resume`, and `write_stdin`/`wait_agent`.
fn retired_targets_in_tool_call(harness: Harness, payload: &Value) -> Vec<String> {
    use cv_core::tools::classify::{visit_tool_ops, BgAction, ClassifyOpts, ToolOp};
    let Some(tool_name) = payload.get("tool_name").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(tool_input) = payload.get("tool_input") else {
        return Vec::new();
    };
    let mut targets = Vec::new();
    let _ = visit_tool_ops(
        Some(harness),
        tool_name,
        cv_core::json::JsonView::from(tool_input),
        ClassifyOpts::FULL,
        |sem| match sem.op {
            ToolOp::AgentMessage { recipient: Some(to) } => targets.push(to.into_owned()),
            ToolOp::Background {
                job_id: Some(id),
                action: BgAction::Interact | BgAction::Poll,
            } => targets.push(id.into_owned()),
            ToolOp::SubagentSpawn {
                target_hint: Some(hint),
                ..
            } => targets.push(hint.into_owned()),
            _ => {}
        },
    );
    if let Some(resume) = tool_input.get("resume").and_then(Value::as_str) {
        targets.push(resume.to_string());
    }
    targets
}

/// `PreToolUse` (every tool, opt-in): refuse the call when the agent making it is retired, or when
/// it messages / resumes a retired agent across any harness. Both answers name the successor.
/// Nothing printed = allow.
fn hook_retired(ev: &Event, payload: &Value) -> Result<Option<Value>> {
    if ev.event.as_deref() != Some("PreToolUse") {
        return Ok(None);
    }
    let retired = cv_core::retired::current();
    if retired.is_empty() {
        return Ok(None);
    }
    let reason = if let Some(r) = ev
        .agent
        .as_ref()
        .and_then(|a| retired.get(cv_core::retired::bare(&a.id)))
        .or_else(|| retired.get(cv_core::retired::bare(&ev.session.id)))
    {
        let next = match &r.successor {
            Some(s) => format!("Your successor {s} continues your work; leave it to that agent."),
            None => "Another agent continues your work.".to_string(),
        };
        format!(
            "cv: you are agent {id}, retired at {at} (`cv distill` replaced you). {next} Make no further tool calls: \
             end your turn now with one line saying you stood down. (The user or the root can reverse this with `cv retire --undo {id}`.)",
            id = r.agent_id,
            at = r.ts.format("%Y-%m-%d %H:%M UTC"),
        )
    } else if let Some((to, r)) = retired_targets_in_tool_call(ev.harness, payload)
        .into_iter()
        .find_map(|to| retired.get(cv_core::retired::bare(&to)).map(|r| (to, r)))
    {
        let next = match &r.successor {
            Some(s) => format!("its successor is {s}. Send this message to {s} instead, and use that id from now on"),
            None => "it has no named successor; ask the root which agent holds its work".to_string(),
        };
        format!("cv: {to} is retired (`cv distill` replaced it); {next}. A message to {to} would wake a second copy of the same lane.")
    } else {
        return Ok(None);
    };
    Ok(Some(json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": reason,
    }})))
}

fn sha256_hex(bytes: &[u8]) -> String {
    cv_core::digest::sha256_hex(bytes)
}

/// One receipt line's chain hash (`sha256("cv-receipt-v1\n{prev}\n{canonical(body)}")`).
#[cfg(test)]
pub(crate) fn receipt_line_hash(prev: &str, body: &Value) -> String {
    cv_core::state::tables::compute_receipt_hash(prev, body)
}

/// Commit a hash-chained tool receipt transactionally into `state.db` (`receipts` + `capture_inbox`).
fn commit_receipt(ev: &Event, body: Map<String, Value>) -> Result<cv_core::state::ReceiptRecord> {
    let store = cv_core::state::StateStore::default_store().context("locating state.db for receipt log")?;
    let rec = store
        .append_receipt(&ev.session.id, ev.harness.as_str(), body)
        .context("appending receipt to state.db")?;
    let op_id = format!("receipt:{}:{}:{}", ev.harness.as_str(), rec.session_id, rec.seq);
    let envelope = json!({
        "kind": "tool_receipt",
        "session_id": rec.session_id,
        "harness": rec.harness,
        "seq": rec.seq,
        "tool_use_id": rec.tool_use_id,
        "tool_name": rec.tool_name,
        "receipt_kind": rec.kind,
        "digest_sha256": rec.digest_sha256,
        "prev_hash": rec.prev_hash,
        "receipt_hash": rec.receipt_hash,
    });
    store
        .append(&op_id, &rec.receipt_hash, &envelope)
        .context("recording receipt in state.db capture_inbox")?;
    Ok(rec)
}

/// `PreToolUse` / `PostToolUse` on every tool: the receipt log in `state.db`. `pre` rows commit
/// the call's input digest durably before the harness forwards the call; `post` rows commit the
/// result's. With `CV_RECEIPT_STRICT=1` a `pre` row that cannot be committed DENIES the call (the
/// chokepoint); without it the call proceeds and the missing link is what a verifier counts.
fn hook_receipt(ev: &Event, payload: &Value) -> Result<Option<Value>> {
    let kind = match ev.event.as_deref() {
        Some("PreToolUse") => "pre",
        Some("PostToolUse") => "post",
        _ => return Ok(None),
    };
    let field = |k: &str| payload.get(k).cloned().unwrap_or(Value::Null);
    let mut body = Map::new();
    body.insert("v".into(), json!(1));
    body.insert("kind".into(), json!(kind));
    body.insert("session".into(), json!(ev.session.id));
    body.insert("agent".into(), json!(ev.agent.as_ref().map(|a| a.id.clone())));
    body.insert("tool_use_id".into(), field("tool_use_id"));
    body.insert("tool".into(), field("tool_name"));
    body.insert("ts".into(), json!(chrono::Utc::now().to_rfc3339()));
    if kind == "pre" {
        body.insert(
            "input_sha256".into(),
            json!(sha256_hex(
                cv_core::state::tables::canonical_json(&field("tool_input")).as_bytes()
            )),
        );
    } else {
        body.insert(
            "response_sha256".into(),
            json!(sha256_hex(
                cv_core::state::tables::canonical_json(&field("tool_response")).as_bytes()
            )),
        );
    }
    match commit_receipt(ev, body) {
        Ok(_) => Ok(None),
        Err(e) if kind == "pre" && cv_core::env::var("CV_RECEIPT_STRICT").as_deref() == Ok("1") => {
            Ok(Some(json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": format!("cv receipt: the call could not be committed before it ran ({e:#}); CV_RECEIPT_STRICT=1 refuses unreceipted calls"),
            }})))
        }
        Err(e) => Err(e),
    }
}

/// `UserPromptSubmit`: one line when decisions are owed, else nothing at all.
fn hook_decisions(_ev: &Event) -> Result<Option<Value>> {
    let Some((who, n)) = owed_decisions(HookName::Decisions) else {
        return Ok(None);
    };
    Ok(Some(context(
        "UserPromptSubmit",
        format!("cv: {n} decision(s) owed to {who} — `cv task inbox` lists them; `cv task resolve <id> --accept-default` or `--choice <option>` answers one."),
    )))
}

fn minutes_since_modified(path: &Path) -> Option<i64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(std::time::SystemTime::now().duration_since(m).ok()?.as_secs() as i64 / 60)
}

/// `PreToolUse` (`Edit|Write`, Codex `apply_patch`): the harness's guard decision when another
/// live writer edited this path. Both `events::sessions_editing` and `ev.normalized_edits()` share
/// [`cv_core::tools::classify::normalize_path`], so symlinked ancestors (`/var` ↔ `/private/var`)
/// resolve identically even when the target file does not exist yet. Nothing printed = allow.
fn hook_edit(ev: &Event) -> Result<Option<Value>> {
    let Some(guard) = cv_core::hooks::Guard::of(ev) else {
        return Ok(None);
    };
    let mut conflicts = Vec::new();
    for abs_str in ev.normalized_edits() {
        let abs = PathBuf::from(&abs_str);
        let mut live: Vec<(String, i64)> = Vec::new();
        // The guard answers in milliseconds and never blocks an edit on its own failure: an event
        // catalog that cannot answer degrades once, as an accelerator does.
        let editing = cv_core::events::sessions_editing(&abs_str).unwrap_or_else(|e| {
            cv_core::env::warn_once("the event catalog", format_args!("{e:#}"));
            Vec::new()
        });
        for t in editing {
            let Some(path) = cv_core::hooks::transcript_of(&t) else {
                continue;
            };
            if guard.exempts(&t, &path) {
                continue;
            }
            if let Some(m) = minutes_since_modified(&path).filter(|m| *m <= LIVE_MINUTES) {
                let shown = match Harness::parse(&t.harness) {
                    Some(h) => selector(h, &t.session_id),
                    None => short_id(&t.session_id),
                };
                live.push((shown, m));
            }
        }
        if live.is_empty() {
            continue;
        }
        live.sort_by_key(|(_, m)| *m);
        let (first, mins) = &live[0];
        let more = match live.len() {
            1 => String::new(),
            n => format!(" (and {} more live session{})", n - 1, if n == 2 { "" } else { "s" }),
        };
        let shown = home_rel(&abs);
        conflicts.push(format!(
            "cv: {shown} was edited by live session {first} ({mins} min ago){more}. `cv show {first} --last 10` shows its hand; proceed only if the edits do not collide."
        ));
    }
    if conflicts.is_empty() {
        return Ok(None);
    }
    Ok(Some(json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": guard.decision.as_str(),
        "permissionDecisionReason": conflicts.join("\n"),
    }})))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hashes Codex 0.162.1's `hooks/list` reported for cv's six entries as `with_ours` writes
    /// them (recorded 2026-10-19 in an isolated `CODEX_HOME`; the binary path is part of the hash).
    #[test]
    fn codex_hook_hash_matches_codex_0_162_1() {
        let exe = "/private/tmp/claude-501/-Users-ember-dev-cv/ece21c13-ee7c-4db3-b8a6-e83ade448e6a/scratchpad/target-hooks/debug/cv";
        // Recorded for 0.17's command strings; the hash covers the command, so pin those.
        let mut settings = with_ours(Harness::Codex, json!({}), exe);
        for groups in settings["hooks"].as_object_mut().unwrap().values_mut() {
            for group in groups.as_array_mut().unwrap() {
                let handler = &mut group["hooks"][0];
                let name = ours(handler).unwrap();
                handler["command"] = json!(format!("CV_HOOK_HARNESS=codex {exe} hook {}", name.as_str()));
            }
        }
        let recorded = [
            (
                "SessionStart",
                0,
                "sha256:bab06d02d2b21b9460a5e75fe440527fb5133c32ba0bf29307f663ed3e45d1b1",
            ),
            (
                "SessionStart",
                1,
                "sha256:0648814e802b9e8ceaa87e88b9890ea7bfae7c35d03fe595bf8f0f3fdbdf956a",
            ),
            (
                "SubagentStart",
                0,
                "sha256:a374e342343e7317477ef79e00c96e9d54b586c80958b47a766d2e33a5cb48f2",
            ),
            (
                "SubagentStop",
                0,
                "sha256:2213dfa52226988d3a710d6fa6ffa12b83028a189d06968e8386ab9fd397a28a",
            ),
            (
                "UserPromptSubmit",
                0,
                "sha256:72e820dad14de1ced01a772173169e39c3e88bb7d9ac3265e8fb8c838fad1bc5",
            ),
            (
                "PreToolUse",
                0,
                "sha256:aec1ede91be48eb971d19eacb805a8a9b31038fedfcae6c2e460198c8a77b3e6",
            ),
        ];
        for (event, group, hash) in recorded {
            let g = &settings["hooks"][event][group];
            assert_eq!(
                codex_hook_hash(event, g["matcher"].as_str(), &g["hooks"][0]),
                hash,
                "{event}[{group}]"
            );
        }
        // The keys Codex stores trust under, and the three states its `hooks/list` reports.
        let path = Path::new("/home/u/.codex/hooks.json");
        let rows = codex_trust(path, &settings);
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().all(|r| r.trust == Trust::Untrusted));
        assert!(rows
            .iter()
            .any(|r| r.key == "/home/u/.codex/hooks.json:session_start:1:0" && r.name == HookName::Start));
        assert!(rows
            .iter()
            .any(|r| r.key == "/home/u/.codex/hooks.json:pre_tool_use:0:0" && r.hash == recorded[5].2));
    }

    #[test]
    fn retired_guard_installs_alone_survives_a_reinstall_and_removes_alone() {
        let theirs = json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "guard.sh"}]}]}});
        let once = with_retired(Harness::Claude, theirs.clone(), "/opt/cv");
        assert_eq!(
            with_retired(Harness::Claude, once.clone(), "/opt/cv"),
            once,
            "idempotent"
        );
        let group = &once["hooks"]["PreToolUse"][1];
        assert!(group.get("matcher").is_none(), "every tool: {group}");
        assert_eq!(ours(&group["hooks"][0]), Some(HookName::Retired));
        assert!(
            present(Harness::Claude, &once).is_empty(),
            "the guard alone, none of the six"
        );
        // The six install beside it and keep it; removing the guard alone leaves the six.
        let all = with_ours(Harness::Claude, once.clone(), "/opt/cv");
        assert_eq!(present(Harness::Claude, &all).len(), SPECS.len());
        let guards = |v: &Value| {
            v["hooks"]["PreToolUse"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|g| g["hooks"].as_array().unwrap().iter())
                .filter(|h| ours(h) == Some(HookName::Retired))
                .count()
        };
        assert_eq!(guards(&all), 1);
        let mut no_guard = all.clone();
        strip_where(&mut no_guard, |n| n == HookName::Retired);
        assert_eq!(guards(&no_guard), 0);
        assert_eq!(present(Harness::Claude, &no_guard).len(), SPECS.len());
        let mut back = once;
        strip_ours(&mut back);
        assert_eq!(back, theirs, "remove deletes the guard too, and nothing of theirs");
    }

    #[test]
    fn receipt_log_installs_on_both_events_and_survives_a_reinstall() {
        let theirs =
            json!({"hooks": {"PostToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "x.sh"}]}]}});
        let once = with_receipts(Harness::Claude, theirs.clone(), "/opt/cv");
        assert_eq!(
            with_receipts(Harness::Claude, once.clone(), "/opt/cv"),
            once,
            "idempotent"
        );
        let count = |v: &Value, ev: &str| {
            v["hooks"][ev]
                .as_array()
                .map(|gs| {
                    gs.iter()
                        .flat_map(|g| g["hooks"].as_array().unwrap().iter())
                        .filter(|h| ours(h) == Some(HookName::Receipt))
                        .count()
                })
                .unwrap_or(0)
        };
        assert_eq!((count(&once, "PreToolUse"), count(&once, "PostToolUse")), (1, 1));
        let all = with_ours(Harness::Claude, once.clone(), "/opt/cv");
        assert_eq!(
            (count(&all, "PreToolUse"), count(&all, "PostToolUse")),
            (1, 1),
            "the six keep it"
        );
        let mut back = once;
        strip_where(&mut back, |n| n == HookName::Receipt);
        assert_eq!(back, theirs, "remove --receipts deletes only its two entries");
    }

    #[test]
    fn receipt_lines_chain_and_any_edit_moves_the_hash() {
        let a = json!({"v": 1, "kind": "pre", "tool_use_id": "t1", "input_sha256": "00", "prev": ""});
        let ha = receipt_line_hash("", &a);
        let b = json!({"v": 1, "kind": "pre", "tool_use_id": "t2", "input_sha256": "01", "prev": ha});
        let hb = receipt_line_hash(&ha, &b);
        assert_ne!(ha, hb);
        let mut a2 = a.clone();
        a2["input_sha256"] = json!("02");
        assert_ne!(
            receipt_line_hash("", &a2),
            ha,
            "an edited input digest moves the line hash"
        );
        assert_ne!(
            receipt_line_hash("x", &a),
            ha,
            "a different predecessor moves the line hash"
        );
        // Key order does not: the hash is over canonical JSON.
        let reordered: Value =
            serde_json::from_str(r#"{"prev":"","input_sha256":"00","tool_use_id":"t1","kind":"pre","v":1}"#).unwrap();
        assert_eq!(receipt_line_hash("", &reordered), ha);
    }

    #[test]
    fn hook_install_keeps_existing_readers_on_the_previous_files() {
        use std::io::Read;
        let _world = crate::test_world("hooks-atomic");
        let (settings, skill) = paths(Harness::Claude, Scope::User).unwrap();
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&settings, "{}").unwrap();
        std::fs::write(&skill, "previous skill").unwrap();
        let mut previous_settings = std::fs::File::open(&settings).unwrap();
        let mut previous_skill = std::fs::File::open(&skill).unwrap();
        cmd_hooks(HooksCmd::Install {
            harness: "claude".into(),
            scope: Scope::User,
            retired: false,
            receipts: false,
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
            with_ours(Harness::Claude, json!({}), &this_exe().unwrap())
        );
        assert_eq!(std::fs::read_to_string(skill).unwrap(), SKILL);
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
        let once = with_ours(Harness::Claude, theirs.clone(), "/opt/cv");
        let twice = with_ours(Harness::Claude, once.clone(), "/opt/cv");
        assert_eq!(once, twice, "a second install changes nothing");
        assert_eq!(present(Harness::Claude, &once).len(), SPECS.len());
        // Theirs survived, in place.
        assert_eq!(once["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(once["hooks"]["PreToolUse"][1]["matcher"], "Edit|Write");
        assert_eq!(once["hooks"]["SubagentStop"][0]["hooks"][0]["async"], true);
        assert!(once["hooks"]["SessionStart"][0]["hooks"][0]["async"].is_null());
        // A reinstall from another binary path replaces ours rather than adding a seventh.
        let moved = with_ours(Harness::Claude, once.clone(), "/usr/local/bin/cv");
        assert_eq!(present(Harness::Claude, &moved).len(), SPECS.len());
        assert_eq!(moved["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        let mut back = moved;
        strip_ours(&mut back);
        assert_eq!(back, theirs, "remove restores the original exactly");
        // Nothing of ours in an empty file leaves no `hooks` key behind.
        let mut empty = with_ours(Harness::Claude, json!({}), "/opt/cv");
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
        assert_eq!(
            ours(&json!({"command": "/opt/cv hook edit --harness codex"})),
            Some(HookName::Edit)
        );
        assert_eq!(ours(&json!({"command": "/opt/cv hook edit --other x"})), None);
        assert_eq!(
            command_for(Harness::Codex, "/opt/cv", HookName::Lane),
            "/opt/cv hook lane --harness codex"
        );
        assert_eq!(shell_quote("/a b/cv"), "'/a b/cv'");
        assert_eq!(shell_quote("/usr/local/bin/cv"), "/usr/local/bin/cv");
        assert_eq!(shell_quote("~/bin/cv"), "'~/bin/cv'", "a bare leading ~ would expand");
        assert_eq!(shell_quote("/opt/cv@1/cv"), "/opt/cv@1/cv");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    fn claude_tail(p: &Path) -> Result<Option<(Option<String>, Option<u64>)>> {
        cv_core::harness::rootless(Harness::Claude)
            .unwrap()
            .last_compact_boundary(p, HOOK_READ_BYTES)
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
        assert_eq!(claude_tail(&p).unwrap(), Some((Some("auto".into()), Some(180_412))));
        std::fs::write(&p, format!("{}\n{quote}\n", b("auto", 5))).unwrap();
        assert_eq!(claude_tail(&p).unwrap(), Some((Some("auto".into()), Some(5))));
        std::fs::write(&p, format!("{quote}\n")).unwrap();
        assert_eq!(claude_tail(&p).unwrap(), None);
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
        assert_eq!(last_commit(&p, HOOK_READ_BYTES), " · last commit 0abec0f");
        // None in the tail it read of a longer transcript: it says how far back it looked.
        assert_eq!(last_commit(&p, 64), " · no commit in its last 64 B");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn last_commit_guards_binary_sqlite_and_compressed_stores_from_raw_byte_tailing() {
        let _world = crate::test_world("hooks-binary-tail-guard");
        let dir = std::env::temp_dir().join(format!("cv-hooks-bin-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();

        // 1. Multi-session Hermes SQLite DB: another session has a commit at the end of the DB,
        // while our target session has its own commit (and a quiet session has none).
        let db_path = dir.join("state.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT, started_at REAL, ended_at REAL, message_count INTEGER, title TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, role TEXT, content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL);
             INSERT INTO sessions VALUES ('target-sess', 'm', 100.0, 101.0, 2, 'target');
             INSERT INTO sessions VALUES ('quiet-sess', 'm', 100.0, 101.0, 1, 'quiet');
             INSERT INTO sessions VALUES ('other-sess', 'm', 102.0, 103.0, 1, 'other');
             INSERT INTO messages (session_id, role, content, tool_call_id, tool_name, timestamp)
                 VALUES ('target-sess', 'tool', '[main a1b2c3d4] target commit\n 1 file changed', 'tc1', 'bash', 100.5);
             INSERT INTO messages (session_id, role, content, timestamp)
                 VALUES ('quiet-sess', 'user', 'just reading', 100.5);
             INSERT INTO messages (session_id, role, content, tool_call_id, tool_name, timestamp)
                 VALUES ('other-sess', 'tool', '[other 99999999] wrong session commit\n', 'tc2', 'bash', 102.5);",
        )
        .unwrap();
        drop(conn);

        // Raw `tail_bytes` / `last_commit` refuses binary SQLite files (even with tiny byte cap).
        assert!(tail_bytes(&db_path, 64).is_none());
        assert_eq!(last_commit(&db_path, 64), "");

        // Session-aware lookup extracts ONLY that session's commit via the adapter, never leaking
        // `other-sess`'s commit or reporting DB file size as "no commit in its last ...".
        let target_ref = SessionRef::new("target-sess", Harness::Hermes, &db_path);
        assert_eq!(
            last_commit_session(&target_ref, HOOK_READ_BYTES),
            " · last commit a1b2c3d"
        );
        let quiet_ref = SessionRef::new("quiet-sess", Harness::Hermes, &db_path);
        assert_eq!(last_commit_session(&quiet_ref, 64), "");

        // 2. Compressed `.zst` store (zstd magic bytes + fake `[leak deadbeef] ` suffix):
        // raw `tail_bytes` / `last_commit` never reads raw bytes from `.zst` or zstd-magic files.
        let zst_path = dir.join("rollout.jsonl.zst");
        let mut fake_zst = vec![0x28, 0xB5, 0x2F, 0xFD];
        fake_zst.extend_from_slice(&vec![b'x'; 256]);
        fake_zst.extend_from_slice(b"[leak deadbeef] raw tail must never match\n");
        std::fs::write(&zst_path, &fake_zst).unwrap();
        assert!(tail_bytes(&zst_path, HOOK_READ_BYTES).is_none());
        assert_eq!(last_commit(&zst_path, 64), "");

        let magic_only = dir.join("extensionless-compressed");
        std::fs::write(&magic_only, &fake_zst).unwrap();
        assert!(tail_bytes(&magic_only, HOOK_READ_BYTES).is_none());
        assert_eq!(last_commit(&magic_only, 64), "");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hook_receipt_honors_scoped_cv_receipt_strict_env_override() {
        let dir = std::env::temp_dir().join(format!("cv-hooks-strict-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        // Make `CLUSTERVISION_HOME` a regular file so creating `<cv_home>/receipts` fails deterministically.
        let blocked_home = dir.join("not-a-dir");
        std::fs::write(&blocked_home, "file blocking directory creation").unwrap();

        let ev = Event {
            harness: Harness::Claude,
            event: Some("PreToolUse".into()),
            session: SessionRef::new("s-strict", Harness::Claude, dir.join("s.jsonl")),
            cwd: None,
            agent: None,
            edits: Vec::new(),
        };
        let payload = json!({
            "tool_use_id": "tu-1",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"},
        });

        // Without CV_RECEIPT_STRICT=1, a failed receipt write surfaces Err.
        {
            let _scope = cv_core::env::Env::from_vars([("CLUSTERVISION_HOME", blocked_home.as_os_str())]).enter();
            assert!(hook_receipt(&ev, &payload).is_err());
        }

        // With scoped CV_RECEIPT_STRICT=1 (without touching std::env), PreToolUse returns a structured deny!
        {
            let _scope = cv_core::env::Env::from_vars([
                ("CLUSTERVISION_HOME", blocked_home.as_os_str()),
                ("CV_RECEIPT_STRICT", std::ffi::OsStr::new("1")),
            ])
            .enter();
            let resp = hook_receipt(&ev, &payload).unwrap().expect("must return deny decision");
            assert_eq!(resp["hookSpecificOutput"]["permissionDecision"], "deny");
            assert!(resp["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("CV_RECEIPT_STRICT=1"));
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
