//! `cv schema` — the reference. Bare: the `-q` query-calculus reference (`cv_core::query`).
//! `--json`: the machine-readable schema of every shape cv emits (session row, session, message,
//! block, query). `--commands`: the command tree, produced from clap's own introspection so
//! names, flags, and help can't drift from the binary.

use anyhow::Result;
use cv_core::query;
use serde_json::{json, Value};
use std::any::TypeId;
use std::ffi::OsString;
use std::path::PathBuf;

pub(crate) fn cmd_schema(cli: &clap::Command, json: bool, commands: bool) -> Result<()> {
    if commands {
        if json {
            println!("{}", serde_json::to_string_pretty(&commands_json(cli))?);
        } else {
            for (group, names) in crate::GROUPS {
                println!("{group}");
                for name in names.iter() {
                    if let Some(sub) = cli.get_subcommands().find(|s| s.get_name() == *name) {
                        let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
                        println!("  {name:<11} {about}");
                    }
                }
            }
            println!("\n(`cv schema --commands --json` for the full tree with every argument)");
        }
        return Ok(());
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&data_schema_json())?);
    } else {
        print!("{}", query::reference());
        println!(
            "\nMachine-readable: `cv schema --json` (data shapes) · `cv schema --commands --json` (the command tree)"
        );
    }
    Ok(())
}

/// The data shapes: what `--json` outputs are made of. Field lists mirror `cv_core::ir`; the
/// block `type` tag and the message `kind`/`origin` vocabularies are enumerated so a consumer can
/// branch without reading Rust. The vocabularies are the IR's own lists (`*_NAMES`, beside the
/// enums), so a renamed/removed variant fails to compile and a new one is published automatically.
pub(crate) fn data_schema_json() -> Value {
    json!({
        "session_row": {
            "description": "one session in a list — identical across `ls --json` and `search --json`",
            "keys": cv_core::rows::SESSION_ROW_KEYS,
            "enrich_keys": ["display_title", "git", "enrich_error"],
            "search_extra_keys": &cv_core::rows::SEARCH_ROW_KEYS[cv_core::rows::SESSION_ROW_KEYS.len()..],
            "timestamps": "RFC 3339 strings, null when unknown",
        },
        "subagent_row": {
            "description": "one sub-agent of a session's forest (`/api/session/{harness}/{id}/subagents`): the session row plus its sidecar facts",
            "keys": cv_core::rows::SUBAGENT_ROW_KEYS,
        },
        "task_row": {
            "description": "one task in a list (`task list --json`, `/api/tasks`); `detail=1` adds the backlog columns, `tags`/`blocked_by` ride only when non-empty",
            "keys": cv_core::task::TaskRow::KEYS,
        },
        "session": {
            "description": "the IR `Session` (`show --json`, `export --format json`)",
            "fields": ["id", "harness", "principal", "cwd", "title", "created_at", "updated_at", "model", "git",
                       "system_prompt", "lineage", "messages", "source_path", "extra"],
            "principal": "optional opaque string or object; missing/null means unspecified",
            "lineage": ["forked_from", "parent", "spawned_by_tool_use", "continued_in", "continues", "agent_path"],
            "extra": "harness-specific facts nested under the harness name: extra[\"claude\"][…]; never flat",
        },
        "message": {
            "fields": ["id", "parent_id", "role", "kind", "origin", "timestamp", "model", "content", "usage", "extra"],
            "role": &cv_core::ir::ROLE_NAMES[..],
            "kind": &cv_core::ir::MESSAGE_KIND_NAMES[..],
            "origin": &cv_core::ir::ORIGIN_NAMES[..],
            "usage": ["input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens",
                      "reasoning_tokens", "cost_usd"],
        },
        "block": {
            "tag": "type",
            "types": {
                "text": ["text"],
                "thinking": ["text", "signature", "encrypted", "redacted"],
                "tool_use": ["id", "name", "input", "namespace"],
                "tool_result": ["tool_use_id", "content", "is_error", "tool_name", "status", "details"],
                "image": ["media_type", "data_ref"],
                "file": ["mime", "path", "source"],
            },
        },
        "prompt_row": {
            "description": "one row of `show --kind prompt,answer --json`: what the person typed, or an AskUserQuestion answer",
            "keys": ["index", "timestamp", "kind", "text"],
            "kind": ["prompt", "answer"],
        },
        "lane": {
            "description": "one sub-agent in `lanes --json`, launch order",
            "keys": ["agent_id", "session_id", "path", "agent_type", "description", "tool_use_id", "workflow", "model",
                     "started_at", "last_turn_at", "duration_ms", "messages", "tool_calls", "tokens", "status",
                     "status_source", "last_text", "last_tool", "stranded", "failure_cause", "resets_at", "failure_detail",
                     "endpoint", "endpoint_source", "tasks"],
            "tokens": ["calls", "input", "cache_read", "cache_write", "output", "total"],
            "status": "running · returned · completed · stopped · failed · killed · or a journaled workflow status (done / partial / …)",
            "status_source": ["journal", "task_notification", "subagent_stop", "hook_ledger", "transcript"],
            "stranded": "status completed/stopped AND the last text's final sentences say it is waiting",
            "failure_cause": "failed/killed lanes only, from the transcript's last uncleared API-error notice: rate-limited · context · stopped (absent = undetermined)",
            "endpoint": "the CV_ENDPOINT the lane's own shell commands exported first (lane:<name>; an Agent prompt or a written brief does not count), else with --tasks a lane:<slug> assignee matching the description's leading token",
            "endpoint_source": ["transcript", "description"],
            "tasks": "with --tasks: [{id, title, state, last_note?, last_ts}] the endpoint holds (assignee), newest first",
            "parent_id": "with --recent only: the session whose forest the lane belongs to",
        },
        "query": query::schema_json(),
        "routes": routes(),
    })
}

/// Every method/path pattern `cv serve` answers, from the routers' own tables (`cv_core::routes!`).
fn routes() -> Vec<&'static str> {
    let mut all = [
        cv_core::api::ROUTES,
        super::serve::ROUTES,
        super::serve::EXPORT_ROUTES,
        super::serve_tasks::ROUTES,
    ]
    .concat();
    all.sort_unstable();
    all
}

/// The command tree in the §6 shape: one entry per visible command and subcommand
/// (`"task open"`), each `{ name, group, about, args: [{ name, kind, value_type, possible_values?,
/// default?, help, required }] }`.
pub(crate) fn commands_json(cli: &clap::Command) -> Value {
    // Built, so the groups an `#[arg(group = …)]` names exist on each command (see push_command).
    let mut cli = cli.clone();
    cli.build();
    let mut out = Vec::new();
    for sub in cli.get_subcommands().filter(|s| visible(s)) {
        let group = crate::group_of(sub.get_name()).unwrap_or("");
        push_command(&mut out, sub, None, group);
    }
    Value::Array(out)
}

fn push_command(out: &mut Vec<Value>, cmd: &clap::Command, parent: Option<&str>, group: &str) {
    let name = match parent {
        Some(p) => format!("{p} {}", cmd.get_name()),
        None => cmd.get_name().to_string(),
    };
    // An argument in a group is never required on its own: `required` on a grouped arg (`task
    // resolve --choice` in the `answer` group) means "one of the group", which clap enforces and a
    // per-argument `required` would misstate as "always this one". The derive's own group per
    // variant holds every argument; that one says nothing and is skipped.
    let all_ids: Vec<&str> = cmd
        .get_arguments()
        .filter(|a| !a.is_global_set())
        .map(|a| a.get_id().as_str())
        .collect();
    let grouped: Vec<&str> = cmd
        .get_groups()
        .filter(|g| g.get_args().count() < all_ids.iter().filter(|id| !matches!(**id, "help" | "version")).count())
        .flat_map(|g| g.get_args().map(|id| id.as_str()))
        .collect();
    let args: Vec<Value> = cmd
        .get_arguments()
        .filter(|a| !a.is_hide_set())
        .filter(|a| !matches!(a.get_id().as_str(), "help" | "version"))
        .map(|a| {
            let mut v = arg_json(a);
            if grouped.contains(&a.get_id().as_str()) {
                v["required"] = json!(false);
            }
            v
        })
        .collect();
    out.push(json!({
        "name": name,
        "group": group,
        "about": cmd.get_about().map(|a| a.to_string()).unwrap_or_default(),
        "args": args,
    }));
    for sub in cmd.get_subcommands().filter(|s| visible(s)) {
        push_command(out, sub, Some(&name), group);
    }
}

/// A subcommand the tree reports: not hidden, and not the `help` clap's build adds to every parent.
fn visible(cmd: &clap::Command) -> bool {
    !cmd.is_hide_set() && cmd.get_name() != "help"
}

fn arg_json(a: &clap::Arg) -> Value {
    let takes_values = a.get_action().takes_values();
    let kind = if a.is_positional() {
        "positional"
    } else if takes_values {
        "option"
    } else {
        "flag"
    };
    let mut v = json!({
        "name": a.get_id().as_str(),
        "kind": kind,
        "value_type": value_type(a),
        "help": a
            .get_long_help()
            .or(a.get_help())
            .map(|h| h.to_string())
            .unwrap_or_default(),
        "required": a.is_required_set(),
    });
    let obj = v.as_object_mut().expect("json object");
    if a.is_global_set() {
        obj.insert("global".into(), json!(true));
    }
    if let Some(long) = a.get_long() {
        obj.insert("long".into(), json!(long));
    }
    if matches!(a.get_action(), clap::ArgAction::Append) || a.get_num_args().is_some_and(|n| n.max_values() > 1) {
        obj.insert("multiple".into(), json!(true));
    }
    if takes_values {
        let possible: Vec<String> = a
            .get_possible_values()
            .iter()
            .filter(|p| !p.is_hide_set())
            .map(|p| p.get_name().to_string())
            .collect();
        if !possible.is_empty() {
            obj.insert("possible_values".into(), json!(possible));
        }
        if let Some(d) = a.get_default_values().first() {
            obj.insert("default".into(), json!(d.to_string_lossy()));
        }
    }
    v
}

/// The JSON-schema-ish scalar type of an argument's values, from clap's value parser.
fn value_type(a: &clap::Arg) -> &'static str {
    if !a.get_action().takes_values() {
        return "boolean";
    }
    let t = a.get_value_parser().type_id();
    if t == TypeId::of::<PathBuf>() {
        "path"
    } else if t == TypeId::of::<String>() || t == TypeId::of::<OsString>() {
        "string"
    } else if t == TypeId::of::<bool>() {
        "boolean"
    } else if t == TypeId::of::<usize>()
        || t == TypeId::of::<u64>()
        || t == TypeId::of::<u32>()
        || t == TypeId::of::<u16>()
        || t == TypeId::of::<u8>()
        || t == TypeId::of::<i64>()
        || t == TypeId::of::<i32>()
        || t == TypeId::of::<i16>()
        || t == TypeId::of::<i8>()
    {
        "integer"
    } else if t == TypeId::of::<f64>() || t == TypeId::of::<f32>() {
        "number"
    } else {
        "string"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::ir::{MessageKind, Origin, Role};

    /// The published vocabularies ARE the enums, both ways: every published name deserializes
    /// (so `as_str` cannot drift from serde's spelling) and every variant serializes to a
    /// published name (the lists derive from `ALL`, so a new variant cannot dodge the schema).
    #[test]
    fn kinds_and_origins_round_trip() {
        for k in MessageKind::ALL {
            let name = k.as_str();
            assert_eq!(serde_json::to_value(k).unwrap(), json!(name), "kind {name}");
            serde_json::from_value::<MessageKind>(json!(name)).unwrap_or_else(|e| panic!("kind {name}: {e}"));
        }
        for o in Origin::ALL {
            let name = o.as_str();
            assert_eq!(serde_json::to_value(o).unwrap(), json!(name), "origin {name}");
            serde_json::from_value::<Origin>(json!(name)).unwrap_or_else(|e| panic!("origin {name}: {e}"));
        }
        for r in Role::ALL {
            let name = r.as_str();
            assert_eq!(serde_json::to_value(r).unwrap(), json!(name), "role {name}");
            serde_json::from_value::<Role>(json!(name)).unwrap_or_else(|e| panic!("role {name}: {e}"));
        }
        // The default-by-role table the contract fixes.
        assert_eq!(MessageKind::for_role(Role::User), MessageKind::Prompt);
        assert_eq!(MessageKind::for_role(Role::System), MessageKind::Notice);
    }

    #[test]
    fn prompt_row_reference_names_a_live_command() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let schema = data_schema_json();
                let description = schema["prompt_row"]["description"].as_str().unwrap();
                let command = description.split('`').nth(1).unwrap();
                let mut tree = crate::build_cli();
                tree.build();
                let mut words = command.split_whitespace();
                let name = words.next().unwrap();
                assert!(tree.get_subcommands().any(|c| c.get_name() == name && !c.is_hide_set()));
                tree.try_get_matches_from(["cv", name, "session-id"].into_iter().chain(words))
                    .unwrap();
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// `docs/INTERFACE-V2.md` §1 (the command groups) is generated from the clap tree between
    /// `<!-- begin: … -->`/`<!-- end: … -->` markers, so the contract cannot state a command set
    /// the binary does not have. `CV_REGEN_CONTRACT=1` rewrites it.
    #[test]
    fn contract_tables_are_the_clap_tree() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024) // a debug clap tree overflows the 2 MB test stack
            .spawn(contract_tables_body)
            .unwrap()
            .join()
            .unwrap();
    }

    fn contract_tables_body() {
        let tree = crate::build_cli();
        let commands = commands_json(&tree);
        let commands = commands.as_array().unwrap();
        let name = |c: &Value| c["name"].as_str().unwrap().to_string();
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        let mut subs: Vec<(String, Vec<String>)> = Vec::new();
        for c in commands {
            let (n, g) = (name(c), c["group"].as_str().unwrap().to_string());
            match n.split_once(' ') {
                Some((parent, sub)) => match subs.iter_mut().find(|(p, _)| p == parent) {
                    Some((_, v)) => v.push(sub.to_string()),
                    None => subs.push((parent.to_string(), vec![sub.to_string()])),
                },
                None => match groups.iter_mut().find(|(have, _)| *have == g) {
                    Some((_, v)) => v.push(n),
                    None => groups.push((g, vec![n])),
                },
            }
        }
        let ticks = |v: &[String]| v.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(" ");
        let mut s1 = String::from("| group | commands |\n|---|---|\n");
        for (g, v) in &groups {
            s1.push_str(&format!("| **{g}** | {} |\n", ticks(v)));
        }
        s1.push_str("\n| command | subcommands |\n|---|---|\n");
        for (p, v) in &subs {
            s1.push_str(&format!("| `{p}` | {} |\n", ticks(v)));
        }

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/INTERFACE-V2.md");
        let doc = std::fs::read_to_string(&path).unwrap();
        let want = splice_marked(&doc, "cv schema --commands", &s1);
        if std::env::var_os("CV_REGEN_CONTRACT").is_some() {
            std::fs::write(&path, &want).unwrap();
        } else {
            assert!(
                doc == want,
                "docs/INTERFACE-V2.md §1 disagrees with the clap tree; regenerate with\n  \
                 CV_REGEN_CONTRACT=1 cargo test -p clustervision --bin cv contract_tables\n\
                 and review the diff (a command that appeared or went is a contract change)"
            );
        }
    }

    /// Per long flag name: each use as (command path, value type, vocabulary, units).
    type FlagUses = std::collections::BTreeMap<
        String,
        Vec<(
            String,
            &'static str,
            Vec<String>,
            std::collections::BTreeSet<&'static str>,
        )>,
    >;

    /// The unit words a flag's value and help speak in; two commands that give one flag name
    /// disjoint units mean two things by it (`--since` a duration here, a message index there).
    fn flag_units(a: &clap::Arg) -> std::collections::BTreeSet<&'static str> {
        let value_name = a
            .get_value_names()
            .and_then(|v| v.first())
            .map(|n| n.as_str().to_lowercase());
        let help = a
            .get_long_help()
            .or(a.get_help())
            .map(|h| h.to_string().to_lowercase())
            .unwrap_or_default();
        let mut units = std::collections::BTreeSet::new();
        match value_name.as_deref() {
            Some("dur" | "duration") => {
                units.insert("duration");
            }
            Some("msg") => {
                units.insert("message");
            }
            _ => {}
        }
        // Whole words only: `file_edit` is an event kind, not a file.
        let words: Vec<&str> = help.split(|c: char| !(c.is_alphanumeric() || c == '_')).collect();
        for (word, unit) in [
            ("directory", "directory"),
            ("file", "file"),
            ("seconds", "seconds"),
            ("message", "message"),
        ] {
            if words.contains(&word) {
                units.insert(unit);
            }
        }
        units
    }

    /// `docs/INTERFACE-V2.md` §2: the same word means the same thing everywhere. Every long flag
    /// that appears on more than one command has one value type, a vocabulary no command widens
    /// past the shared one, and compatible units in its help.
    #[test]
    fn a_repeated_flag_name_has_one_type_and_one_grammar() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let mut tree = crate::build_cli();
                tree.build();
                let mut by_flag = FlagUses::default();
                fn walk(cmd: &clap::Command, path: &str, by_flag: &mut FlagUses) {
                    for a in cmd.get_arguments() {
                        let Some(long) = a.get_long() else { continue };
                        if a.is_hide_set() || !a.get_action().takes_values() || matches!(long, "help" | "version") {
                            continue;
                        }
                        let possible = a
                            .get_possible_values()
                            .iter()
                            .map(|p| p.get_name().to_string())
                            .collect();
                        by_flag.entry(long.to_string()).or_default().push((
                            path.to_string(),
                            value_type(a),
                            possible,
                            flag_units(a),
                        ));
                    }
                    for sub in cmd.get_subcommands().filter(|s| visible(s)) {
                        walk(sub, format!("{path} {}", sub.get_name()).trim(), by_flag);
                    }
                }
                walk(&tree, "", &mut by_flag);
                let mut drift = Vec::new();
                for (flag, uses) in &by_flag {
                    if uses.len() < 2 {
                        continue;
                    }
                    // A path is a string the command reads as a location; `--cwd` stays one word
                    // in the units, not in the clap type.
                    let kind = |t: &'static str| {
                        if t == "path" {
                            "string"
                        } else {
                            t
                        }
                    };
                    let types: std::collections::BTreeSet<_> = uses.iter().map(|u| kind(u.1)).collect();
                    if types.len() > 1 {
                        let rows: Vec<_> = uses.iter().map(|u| format!("{} {}", u.0, u.1)).collect();
                        drift.push(format!("--{flag}: value types differ: {}", rows.join("; ")));
                    }
                    let vocab: std::collections::BTreeSet<&String> = uses.iter().flat_map(|u| u.2.iter()).collect();
                    let widest = uses.iter().map(|u| u.2.len()).max().unwrap_or(0);
                    if !vocab.is_empty() && vocab.len() > widest {
                        drift.push(format!("--{flag}: no command offers the whole vocabulary {vocab:?}"));
                    }
                    for (i, a) in uses.iter().enumerate() {
                        for b in &uses[i + 1..] {
                            if !a.3.is_empty() && !b.3.is_empty() && a.3.is_disjoint(&b.3) {
                                drift.push(format!(
                                    "--{flag}: `{}` speaks {:?}, `{}` speaks {:?}",
                                    a.0, a.3, b.0, b.3
                                ));
                            }
                        }
                    }
                }
                assert!(
                    drift.is_empty(),
                    "one flag word, one grammar (INTERFACE-V2 §2):\n{}",
                    drift.join("\n")
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// `doc` with the text between `<!-- begin: {key} -->` and `<!-- end: {key} -->` replaced.
    fn splice_marked(doc: &str, key: &str, body: &str) -> String {
        let (begin, end) = (format!("<!-- begin: {key} -->\n"), format!("<!-- end: {key} -->"));
        let a = doc.find(&begin).unwrap_or_else(|| panic!("no `{begin}` marker")) + begin.len();
        let b = doc[a..].find(&end).unwrap_or_else(|| panic!("no `{end}` marker")) + a;
        format!("{}{body}{}", &doc[..a], &doc[b..])
    }

    /// Every key list `cv schema --json` publishes for an IR noun is the key set that noun
    /// serializes with every optional field present (§1's discipline for §3's nouns).
    #[test]
    fn published_nouns_are_their_serialized_keys() {
        let schema = data_schema_json();
        let published = |v: &Value| -> std::collections::BTreeSet<String> {
            v.as_array()
                .unwrap()
                .iter()
                .map(|k| k.as_str().unwrap().to_string())
                .collect()
        };
        let keys =
            |v: Value| -> std::collections::BTreeSet<String> { v.as_object().unwrap().keys().cloned().collect() };
        let at = "2026-01-02T03:04:05Z";
        let message = json!({"id": "m", "parent_id": "p", "role": "user", "kind": "prompt", "origin": "human",
            "timestamp": at, "model": "m", "content": [{"type": "text", "text": "t"}],
            "usage": {"input_tokens": 1}, "extra": {"claude": {}}});
        let lineage = json!({"forked_from": "a", "parent": "b", "spawned_by_tool_use": "c", "continued_in": "d",
            "continues": "e", "agent_path": "f"});
        let session = json!({"principal": "p", "id": "s", "harness": "claude", "cwd": "/c", "title": "t",
            "created_at": at, "updated_at": at, "model": "m", "git": {"branch": "b"}, "system_prompt": "s",
            "lineage": lineage, "messages": [message], "source_path": "/s", "extra": {"claude": {}}});
        let parsed: cv_core::Session = serde_json::from_value(session).unwrap();
        assert_eq!(
            keys(serde_json::to_value(&parsed).unwrap()),
            published(&schema["session"]["fields"])
        );
        assert_eq!(
            keys(serde_json::to_value(&parsed.lineage).unwrap()),
            published(&schema["session"]["lineage"])
        );
        assert_eq!(
            keys(serde_json::to_value(&parsed.messages[0]).unwrap()),
            published(&schema["message"]["fields"])
        );

        let at: chrono::DateTime<chrono::Utc> = at.parse().unwrap();
        let some = |s: &str| Some(s.to_string());
        let lane = cv_core::lanes::Lane {
            agent_id: "a".into(),
            session_id: "agent-a".into(),
            path: "/a".into(),
            agent_type: some("t"),
            description: some("d"),
            tool_use_id: some("u"),
            workflow: some("w"),
            model: some("m"),
            started_at: Some(at),
            last_turn_at: Some(at),
            duration_ms: Some(1),
            messages: 1,
            tool_calls: 1,
            tokens: Default::default(),
            status: cv_core::lanes::LaneStatus::Running,
            status_source: cv_core::lanes::StatusSource::Transcript,
            last_text: some("l"),
            last_tool: some("b"),
            stranded: false,
            failure_cause: some("context"),
            resets_at: Some(at),
            failure_detail: some("f"),
            endpoint: some("lane:a"),
            endpoint_source: Some(cv_core::lanes::EndpointSource::Transcript),
            tasks: Some(Vec::new()),
        };
        let lane = serde_json::to_value(&lane).unwrap();
        assert_eq!(keys(lane["tokens"].clone()), published(&schema["lane"]["tokens"]));
        assert_eq!(keys(lane), published(&schema["lane"]["keys"]));
    }

    #[test]
    fn data_schema_names_the_session_row_keys() {
        let v = data_schema_json();
        assert_eq!(v["block"]["tag"], "type");
        assert_eq!(v["session_row"]["keys"].as_array().unwrap().len(), 9);
        assert!(v["query"].is_object());
        let routes: Vec<&str> = v["routes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect();
        for route in [
            "GET /api/sessions",
            "GET /api/session/{}/{}/messages",
            "GET /api/session/{}/{}/export",
            "GET /api/board/*",
            "GET /api/search",
            "POST /api/task/{}/{}",
        ] {
            assert!(routes.contains(&route), "{route} missing from {routes:?}");
        }
        assert!(routes.windows(2).all(|w| w[0] < w[1]), "sorted, unique: {routes:?}");
        assert!(v["lane"]["status_source"]
            .as_array()
            .unwrap()
            .contains(&json!(cv_core::lanes::StatusSource::HookLedger)));
    }
}
