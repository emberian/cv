//! `cv schema` — the reference. Bare: the `-q` query-calculus reference (`cv_core::query`).
//! `--json`: the machine-readable schema of every shape cv emits (session row, session, message,
//! block, query). `--commands`: the command tree — with `--json` in the exact shape `cv-mcp`
//! generates its tools from (docs/INTERFACE-V2.md §6), produced from clap's own introspection so
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

/// Every `MessageKind` as serde spells it. Kept in step with `cv_core::ir::MessageKind` by
/// `kinds_and_origins_round_trip` (each name must deserialize).
pub(crate) const MESSAGE_KINDS: &[&str] = &[
    "prompt",
    "reply",
    "tool_result",
    "injected_context",
    "system_prompt",
    "notice",
    "compaction_boundary",
    "compaction_summary",
    "model_change",
    "error",
    "subagent_spawn",
    "subagent_return",
    "branch",
    "carrier",
];

/// Every `Origin` as serde spells it.
pub(crate) const ORIGINS: &[&str] = &[
    "human",
    "model",
    "harness",
    "hook",
    "scheduler",
    "subagent",
    "import",
    "unknown",
];

pub(crate) const ROLES: &[&str] = &["system", "user", "assistant", "tool"];

/// The data shapes: what `--json` outputs are made of. Field lists mirror `cv_core::ir`; the
/// block `type` tag and the message `kind`/`origin` vocabularies are enumerated so a consumer can
/// branch without reading Rust.
pub(crate) fn data_schema_json() -> Value {
    json!({
        "session_row": {
            "description": "one session in a list — identical across `ls --json`, `search --json`, `timeline --json`",
            "keys": ["id", "harness", "path", "cwd", "title", "created_at", "updated_at", "message_count", "size_bytes"],
            "enrich_keys": ["display_title", "git"],
            "search_extra_keys": ["score", "snippet", "agent_id", "parent_id", "workflow"],
            "timestamps": "RFC 3339 strings, null when unknown",
        },
        "session": {
            "description": "the IR `Session` (`show --json`, `export --format json`)",
            "fields": ["id", "harness", "cwd", "title", "created_at", "updated_at", "model", "git",
                       "system_prompt", "lineage", "messages", "source_path", "extra"],
            "lineage": ["forked_from", "parent", "spawned_by_tool_use", "continued_in", "continues", "agent_path"],
            "extra": "harness-specific facts nested under the harness name: extra[\"claude\"][…]; never flat",
        },
        "message": {
            "fields": ["id", "parent_id", "role", "kind", "origin", "timestamp", "model", "content", "usage", "extra"],
            "role": ROLES,
            "kind": MESSAGE_KINDS,
            "origin": ORIGINS,
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
        "query": query::schema_json(),
    })
}

/// The command tree in the §6 shape: one entry per visible command and subcommand
/// (`"task open"`), each `{ name, group, about, args: [{ name, kind, value_type, possible_values?,
/// default?, help, required }] }`.
pub(crate) fn commands_json(cli: &clap::Command) -> Value {
    let mut out = Vec::new();
    for sub in cli.get_subcommands().filter(|s| !s.is_hide_set()) {
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
    let args: Vec<Value> = cmd
        .get_arguments()
        .filter(|a| !a.is_hide_set())
        .filter(|a| !matches!(a.get_id().as_str(), "help" | "version"))
        .map(arg_json)
        .collect();
    out.push(json!({
        "name": name,
        "group": group,
        "about": cmd.get_about().map(|a| a.to_string()).unwrap_or_default(),
        "args": args,
    }));
    for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
        push_command(out, sub, Some(&name), group);
    }
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

    /// The enumerated vocabularies match the IR's serde spelling (a renamed/removed variant
    /// fails here; a new one must be added to the list to be published).
    #[test]
    fn kinds_and_origins_round_trip() {
        for k in MESSAGE_KINDS {
            serde_json::from_value::<MessageKind>(json!(k)).unwrap_or_else(|e| panic!("kind {k}: {e}"));
        }
        for o in ORIGINS {
            serde_json::from_value::<Origin>(json!(o)).unwrap_or_else(|e| panic!("origin {o}: {e}"));
        }
        for r in ROLES {
            serde_json::from_value::<Role>(json!(r)).unwrap_or_else(|e| panic!("role {r}: {e}"));
        }
        // The default-by-role table the contract fixes.
        assert_eq!(MessageKind::for_role(Role::User), MessageKind::Prompt);
        assert_eq!(MessageKind::for_role(Role::System), MessageKind::Notice);
    }

    #[test]
    fn data_schema_names_the_session_row_keys() {
        let v = data_schema_json();
        assert_eq!(v["block"]["tag"], "type");
        assert_eq!(v["session_row"]["keys"].as_array().unwrap().len(), 9);
        assert!(v["query"].is_object());
    }
}
