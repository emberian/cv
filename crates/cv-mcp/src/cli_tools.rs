//! CLI-generated MCP tools (docs/INTERFACE-V2.md §6).
//!
//! The `cv` CLI is the single source of truth for what cv can do. Rather than hand-copying its
//! surface into `json!` literals — where names, flags, harness lists and help text drift the
//! moment the CLI moves — cv-mcp shells out to `cv schema --commands --json` **once at startup**
//! and builds one MCP tool per command from clap's own introspection.
//!
//! - **Which commands.** Everything clap shows under the **Read**, **Reshape**, **Export** and
//!   **System** groups. `Fleet & live` (`task`, `board`, `scry`, `share`) is excluded: those have
//!   hand-written MCP tools with long-poll/claim semantics a subprocess can't offer.
//! - **Which binary.** `$CV_BIN`, else a `cv` sitting next to the running `cv-mcp`, else `cv` on
//!   `PATH`; the first candidate whose schema dump parses wins (see [`locate_cv`]).
//! - **Calls** run `cv <command> <args…> --json` (the `--json` only for commands that have the
//!   flag) and return stdout. A non-zero exit becomes a tool error carrying stderr.
//! - **Windows.** `show` is the one command whose MCP defaults differ from the CLI's: with no
//!   window selector it gets `--last 50 --max-bytes 200000`, so a full transcript is never what an
//!   agent accidentally pulls into its context.
//!
//! If the dump fails (no `cv`, an old `cv`, unparseable JSON) the failure is logged to stderr and
//! the server still serves the hand-written tools.

use anyhow::{anyhow, Context as _, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// The command groups that become MCP tools. `Fleet & live` is deliberately absent.
const TOOL_GROUPS: [&str; 4] = ["Read", "Reshape", "Export", "System"];

/// `show`'s MCP-only window defaults: applied when the caller names no selector at all.
pub(crate) const SHOW_DEFAULT_LAST: u64 = 50;
pub(crate) const SHOW_DEFAULT_MAX_BYTES: u64 = 200_000;

/// The window selectors from INTERFACE-V2 §2, plus `pre_compaction` (which sets the window
/// itself, so injecting a default on top of it would conflict).
const WINDOW_SELECTORS: [&str; 6] = ["first", "last", "range", "around", "max_bytes", "pre_compaction"];

/// One argument of one command, as `cv schema --commands --json` describes it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ArgSpec {
    /// clap's argument id: snake_case, and the long flag is its kebab-case spelling.
    pub name: String,
    pub kind: ArgKind,
    #[serde(default)]
    pub value_type: String,
    #[serde(default)]
    pub possible_values: Vec<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub help: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ArgKind {
    Flag,
    Option,
    Positional,
}

/// One command of the tree.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CommandSpec {
    /// Space-separated for a subcommand (`"formats census"`); the tool name joins with `_`.
    pub name: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub about: String,
    #[serde(default)]
    pub args: Vec<ArgSpec>,
}

impl ArgSpec {
    /// The long flag clap derives from the argument id (`max_bytes` → `--max-bytes`).
    pub fn long_flag(&self) -> String {
        format!("--{}", self.name.replace('_', "-"))
    }
}

impl CommandSpec {
    /// The MCP tool name: `"formats census"` → `formats_census`.
    pub fn tool_name(&self) -> String {
        self.name.replace(' ', "_")
    }

    fn arg(&self, name: &str) -> Option<&ArgSpec> {
        self.args.iter().find(|a| a.name == name)
    }

    /// Whether `cv <cmd>` understands `--json` (so a call can ask for machine output).
    fn has_json(&self) -> bool {
        self.arg("json").is_some_and(|a| a.kind == ArgKind::Flag)
    }
}

/// The generated half of the tool registry: a located `cv` binary plus the commands it advertises.
pub(crate) struct CliTools {
    bin: PathBuf,
    commands: Vec<CommandSpec>,
}

impl CliTools {
    /// Find a `cv`, dump its command tree, and keep the commands in the four tool groups.
    /// Errors (rather than panicking or exiting) so `main` can degrade to the hand-written tools.
    pub fn discover() -> Result<CliTools> {
        let (bin, dump) = locate_cv()?;
        let all: Vec<CommandSpec> = serde_json::from_str(&dump).context("parsing `cv schema --commands --json`")?;
        let commands = select_tool_commands(all);
        if commands.is_empty() {
            anyhow::bail!("`cv schema --commands --json` listed no Read/Reshape/Export/System commands");
        }
        Ok(CliTools { bin, commands })
    }

    pub fn bin(&self) -> &Path {
        &self.bin
    }

    pub fn commands(&self) -> &[CommandSpec] {
        &self.commands
    }

    pub fn find(&self, tool: &str) -> Option<&CommandSpec> {
        self.commands.iter().find(|c| c.tool_name() == tool)
    }

    /// The MCP tool descriptors for every generated command.
    pub fn tool_list(&self) -> Vec<Value> {
        self.commands.iter().map(tool_descriptor).collect()
    }

    /// Run one generated tool and return the child's stdout.
    pub fn call(&self, tool: &str, args: &Value) -> Result<String> {
        let cmd = self
            .find(tool)
            .ok_or_else(|| anyhow!("unknown generated tool: {tool}"))?;
        let argv = build_argv(cmd, args)?;
        let out = std::process::Command::new(&self.bin)
            .args(&argv)
            .output()
            .with_context(|| format!("running {} {}", self.bin.display(), argv.join(" ")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let detail = if stderr.is_empty() { stdout } else { stderr };
            let code = out
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            anyhow::bail!("cv {} exited {code}: {detail}", argv.join(" "));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Keep the commands in the four tool groups, dropping bare dispatcher parents (a command that
/// has subcommands and no arguments of its own — calling it could only ever print clap's usage).
fn select_tool_commands(all: Vec<CommandSpec>) -> Vec<CommandSpec> {
    let names: Vec<String> = all.iter().map(|c| c.name.clone()).collect();
    all.into_iter()
        .filter(|c| TOOL_GROUPS.contains(&c.group.as_str()))
        .filter(|c| {
            let has_children = names.iter().any(|n| n.starts_with(&format!("{} ", c.name)));
            !(has_children && c.args.is_empty())
        })
        .collect()
}

/// Locate a `cv` binary and return it with its command-tree dump.
///
/// Order: `$CV_BIN` (explicit wins outright), then a `cv` beside the running `cv-mcp` — the pair
/// is built and shipped together, so the sibling is the matching version — then `cv` on `PATH`.
/// A candidate counts only if it actually answers `schema --commands --json`, so an ancient `cv`
/// earlier in the list can't shadow a good one later.
fn locate_cv() -> Result<(PathBuf, String)> {
    let exe_name = if cfg!(windows) { "cv.exe" } else { "cv" };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(explicit) = std::env::var_os("CV_BIN").filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(explicit));
    }
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            candidates.push(dir.join(exe_name));
        }
    }
    candidates.push(PathBuf::from(exe_name)); // PATH lookup

    let mut errors = Vec::new();
    for cand in candidates {
        match schema_dump(&cand) {
            Ok(dump) => return Ok((cand, dump)),
            Err(e) => errors.push(format!("{}: {e:#}", cand.display())),
        }
    }
    Err(anyhow!("no usable `cv` binary ({})", errors.join("; ")))
}

/// `<bin> schema --commands --json`, or an error explaining why that candidate is out.
fn schema_dump(bin: &Path) -> Result<String> {
    let out = std::process::Command::new(bin)
        .args(["schema", "--commands", "--json"])
        .output()
        .context("spawn failed")?;
    if !out.status.success() {
        anyhow::bail!(
            "exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// clap spec → MCP tool descriptor
// ---------------------------------------------------------------------------

/// The MCP descriptor for one command: `{ name, description, inputSchema }`.
pub(crate) fn tool_descriptor(cmd: &CommandSpec) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for a in &cmd.args {
        // `--json` is not the caller's to choose: a generated call always asks for machine output
        // when the command has it.
        if a.name == "json" && a.kind == ArgKind::Flag {
            continue;
        }
        props.insert(a.name.clone(), property_schema(cmd, a));
        if a.required {
            required.push(Value::String(a.name.clone()));
        }
    }
    let mut schema = json!({ "type": "object", "properties": Value::Object(props) });
    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }
    json!({
        "name": cmd.tool_name(),
        "description": tool_description(cmd),
        "inputSchema": schema,
    })
}

/// The tool blurb: clap's `about`, plus the window note on `show` so the defaults are discoverable
/// rather than a surprise in the output.
fn tool_description(cmd: &CommandSpec) -> String {
    let about = if cmd.about.trim().is_empty() {
        format!("`cv {}`.", cmd.name)
    } else {
        cmd.about.trim().to_string()
    };
    let json = if cmd.has_json() { " --json" } else { "" };
    let mut out = format!("{about} (runs `cv {}{json}`).", cmd.name);
    if cmd.tool_name() == "show" {
        out.push_str(&format!(
            " Over MCP this windows by default: with none of first/last/range/around/max_bytes \
             given it reads the LAST {SHOW_DEFAULT_LAST} messages capped at {SHOW_DEFAULT_MAX_BYTES} \
             bytes, so a whole transcript is never pulled in by accident. Ask for a window \
             explicitly to override."
        ));
    }
    out
}

/// One argument as a JSON-schema property.
fn property_schema(cmd: &CommandSpec, a: &ArgSpec) -> Value {
    let mut p = Map::new();
    p.insert("type".into(), json!(json_type(a)));
    p.insert("description".into(), json!(description_of(cmd, a)));
    if !a.possible_values.is_empty() {
        p.insert("enum".into(), json!(a.possible_values));
    }
    if let Some(d) = &a.default {
        // Typed to match the property, so a client that pre-fills defaults sends a valid value.
        p.insert("default".into(), default_value(a, d));
    }
    Value::Object(p)
}

/// clap's value type → a JSON-schema scalar. Paths are strings on the wire; anything cv's schema
/// can't name falls back to `string`, which every client can express.
fn json_type(a: &ArgSpec) -> &'static str {
    if a.kind == ArgKind::Flag {
        return "boolean";
    }
    match a.value_type.as_str() {
        "integer" => "integer",
        "number" => "number",
        "boolean" => "boolean",
        _ => "string", // "string", "path", and anything unrecognised
    }
}

fn default_value(a: &ArgSpec, raw: &str) -> Value {
    match json_type(a) {
        "integer" => raw.parse::<i64>().map(Value::from).unwrap_or_else(|_| json!(raw)),
        "number" => raw.parse::<f64>().map(Value::from).unwrap_or_else(|_| json!(raw)),
        "boolean" => raw.parse::<bool>().map(Value::from).unwrap_or_else(|_| json!(raw)),
        _ => json!(raw),
    }
}

/// clap's help is the description. A few cv arguments carry no help at all (`show --harness`);
/// those get a pointer rather than an empty string, which some MCP clients reject.
fn description_of(cmd: &CommandSpec, a: &ArgSpec) -> String {
    let help = a.help.trim();
    if !help.is_empty() {
        return help.to_string();
    }
    match a.kind {
        ArgKind::Positional => format!("The `<{}>` argument of `cv {}`.", a.name, cmd.name),
        _ => format!("The `{}` option of `cv {}`.", a.long_flag(), cmd.name),
    }
}

// ---------------------------------------------------------------------------
// call arguments → argv
// ---------------------------------------------------------------------------

/// Turn an MCP `arguments` object into the argv after the binary: command words, then positionals
/// in declaration order, then options and flags, then `--json` when the command has it.
pub(crate) fn build_argv(cmd: &CommandSpec, args: &Value) -> Result<Vec<String>> {
    let obj = match args {
        Value::Null => Map::new(),
        Value::Object(o) => o.clone(),
        other => return Err(invalid(format!("arguments must be an object, got {other}"))),
    };

    for key in obj.keys() {
        if key == "json" {
            continue; // forced on; tolerated from a caller that echoes the CLI
        }
        if cmd.arg(key).is_none() {
            return Err(invalid(format!(
                "unknown argument `{key}` for `{}` (see the tool's inputSchema)",
                cmd.tool_name()
            )));
        }
    }
    // A missing required argument is the caller's protocol mistake (the schema said so), not a
    // `cv` failure — answer it the same way whatever kind of argument it is, rather than letting
    // clap's usage text come back as a tool error for options but not for positionals.
    if let Some(a) = cmd.args.iter().find(|a| a.required && present(&obj, &a.name).is_none()) {
        return Err(invalid(format!(
            "missing required argument `{}` for `{}`",
            a.name,
            cmd.tool_name()
        )));
    }

    let mut argv: Vec<String> = cmd.name.split(' ').map(str::to_string).collect();

    // Positionals first and in order: `cat <session> <tool_use_id>` would silently swap its
    // arguments if a hole were left in the middle.
    for a in cmd.args.iter().filter(|a| a.kind == ArgKind::Positional) {
        match present(&obj, &a.name) {
            Some(v) => argv.push(scalar(&a.name, v)?),
            // Required ones were caught above; a hole in the OPTIONAL tail (`workflow <id>
            // [<run_id>]`) just ends the run — skipping it would shift later values into the
            // wrong slot.
            None => break,
        }
    }

    for a in cmd.args.iter().filter(|a| a.kind == ArgKind::Option) {
        let Some(v) = present(&obj, &a.name) else { continue };
        argv.push(a.long_flag());
        argv.push(scalar(&a.name, v)?);
    }

    for a in cmd.args.iter().filter(|a| a.kind == ArgKind::Flag) {
        if a.name == "json" {
            continue;
        }
        match present(&obj, &a.name) {
            Some(Value::Bool(true)) => argv.push(a.long_flag()),
            Some(Value::Bool(false)) | None => {}
            Some(other) => {
                return Err(invalid(format!(
                    "`{}` is a flag: pass true or false, got {other}",
                    a.name
                )))
            }
        }
    }

    apply_mcp_defaults(cmd, &obj, &mut argv);

    if cmd.has_json() {
        argv.push("--json".into());
    }
    Ok(argv)
}

/// `show` without a window selector reads the tail, not the whole transcript (§6).
fn apply_mcp_defaults(cmd: &CommandSpec, obj: &Map<String, Value>, argv: &mut Vec<String>) {
    if cmd.tool_name() != "show" {
        return;
    }
    let windowed = WINDOW_SELECTORS
        .iter()
        .any(|k| cmd.arg(k).is_some() && present(obj, k).is_some());
    if windowed {
        return;
    }
    if cmd.arg("last").is_some() {
        argv.push("--last".into());
        argv.push(SHOW_DEFAULT_LAST.to_string());
    }
    if cmd.arg("max_bytes").is_some() {
        argv.push("--max-bytes".into());
        argv.push(SHOW_DEFAULT_MAX_BYTES.to_string());
    }
}

/// An argument the caller actually supplied (JSON `null` counts as absent).
fn present<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    match obj.get(key) {
        None | Some(Value::Null) => None,
        Some(v) => Some(v),
    }
}

/// A scalar rendered for argv. Numbers keep their JSON spelling minus a trailing `.0`, since clap
/// parses `50` but not `50.0` and some clients serialise every number as a float.
fn scalar(key: &str, v: &Value) -> Result<String> {
    Ok(match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.to_string(),
            None => match n.as_f64() {
                Some(f) if f.fract() == 0.0 => format!("{}", f as i64),
                _ => n.to_string(),
            },
        },
        other => return Err(invalid(format!("`{key}` must be a scalar, got {other}"))),
    })
}

fn invalid(msg: String) -> anyhow::Error {
    crate::InvalidParams(msg).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json_src: &str) -> CommandSpec {
        serde_json::from_str(json_src).expect("command spec")
    }

    /// The real shape of `cv show`, copied from `cv schema --commands --json`.
    fn show() -> CommandSpec {
        spec(
            r#"{
            "name": "show", "group": "Read", "about": "Print a single session",
            "args": [
              {"name":"id","kind":"positional","value_type":"string","help":"Session id","required":true},
              {"name":"harness","kind":"option","value_type":"string","help":"","required":false},
              {"name":"json","kind":"flag","value_type":"boolean","help":"Emit IR JSON","required":false},
              {"name":"first","kind":"option","value_type":"integer","help":"The first N messages","required":false},
              {"name":"last","kind":"option","value_type":"integer","help":"The last N messages","required":false},
              {"name":"range","kind":"option","value_type":"string","help":"A..B","required":false},
              {"name":"around","kind":"option","value_type":"integer","help":"Message N","required":false},
              {"name":"context","kind":"option","value_type":"integer","help":"K either side","required":false,"default":"5"},
              {"name":"max_bytes","kind":"option","value_type":"integer","help":"Stop after N bytes","required":false},
              {"name":"subagents","kind":"flag","value_type":"boolean","help":"List sub-agents","required":false},
              {"name":"pre_compaction","kind":"option","value_type":"integer","help":"Pre-compaction span","required":false}
            ]}"#,
        )
    }

    fn cat() -> CommandSpec {
        spec(
            r#"{
            "name": "cat", "group": "Read", "about": "Print one tool call's full output",
            "args": [
              {"name":"session","kind":"positional","value_type":"string","help":"Session","required":true},
              {"name":"tool_use_id","kind":"positional","value_type":"string","help":"Tool use id","required":true},
              {"name":"input","kind":"flag","value_type":"boolean","help":"Print the input","required":false}
            ]}"#,
        )
    }

    // -- schema translation ------------------------------------------------

    #[test]
    fn positionals_and_options_become_typed_properties() {
        let d = tool_descriptor(&show());
        assert_eq!(d["name"], "show");
        let props = &d["inputSchema"]["properties"];
        assert_eq!(d["inputSchema"]["type"], "object");
        // A positional is a property named after the argument, and required ones are listed.
        assert_eq!(props["id"]["type"], "string");
        assert_eq!(d["inputSchema"]["required"], json!(["id"]));
        // clap help becomes the description; integers stay integers; flags become booleans.
        assert_eq!(props["last"]["type"], "integer");
        assert_eq!(props["last"]["description"], "The last N messages");
        assert_eq!(props["subagents"]["type"], "boolean");
        // A clap default rides along, typed.
        assert_eq!(props["context"]["default"], json!(5));
        // `--json` is forced by the caller-side wrapper, so it is not the agent's to set.
        assert!(props.get("json").is_none(), "json must not be a property: {props}");
    }

    #[test]
    fn empty_clap_help_still_yields_a_description() {
        let d = tool_descriptor(&show());
        let desc = d["inputSchema"]["properties"]["harness"]["description"]
            .as_str()
            .unwrap();
        assert!(desc.contains("--harness"), "{desc}");
        assert!(!desc.is_empty());
    }

    #[test]
    fn possible_values_become_an_enum_and_paths_become_strings() {
        let c = spec(
            r#"{"name":"ls","group":"Read","about":"List","args":[
            {"name":"sort_by","kind":"option","value_type":"string","help":"Sort key","required":false,
             "possible_values":["updated","created","messages"],"default":"updated"},
            {"name":"out","kind":"option","value_type":"path","help":"Where to write","required":false}]}"#,
        );
        let props = tool_descriptor(&c)["inputSchema"]["properties"].clone();
        assert_eq!(props["sort_by"]["enum"], json!(["updated", "created", "messages"]));
        assert_eq!(props["sort_by"]["default"], json!("updated"));
        assert_eq!(props["out"]["type"], "string");
    }

    #[test]
    fn subcommand_names_join_with_underscores_and_dispatchers_are_dropped() {
        let all: Vec<CommandSpec> = serde_json::from_str(
            r#"[{"name":"formats","group":"System","about":"Formats","args":[]},
                {"name":"formats census","group":"System","about":"Census","args":[
                   {"name":"harness","kind":"option","value_type":"string","help":"h","required":false}]},
                {"name":"task","group":"Fleet & live","about":"Tasks","args":[]}]"#,
        )
        .unwrap();
        let kept = select_tool_commands(all);
        let names: Vec<String> = kept.iter().map(|c| c.tool_name()).collect();
        assert_eq!(names, vec!["formats_census"], "dispatcher and Fleet & live are dropped");
    }

    // -- argv construction -------------------------------------------------

    #[test]
    fn show_without_a_selector_gets_the_mcp_window_defaults() {
        let argv = build_argv(&show(), &json!({"id": "abc"})).unwrap();
        assert_eq!(
            argv,
            vec!["show", "abc", "--last", "50", "--max-bytes", "200000", "--json"]
        );
    }

    #[test]
    fn an_explicit_selector_suppresses_the_defaults() {
        for sel in [
            json!({"id": "abc", "first": 10}),
            json!({"id": "abc", "range": "10..20"}),
            json!({"id": "abc", "around": 7, "context": 2}),
            json!({"id": "abc", "max_bytes": 4096}),
            json!({"id": "abc", "pre_compaction": 1}),
        ] {
            let argv = build_argv(&show(), &sel).unwrap();
            assert!(!argv.contains(&"--last".to_string()), "{sel} → {argv:?}");
        }
        let argv = build_argv(&show(), &json!({"id": "abc", "last": 3})).unwrap();
        assert_eq!(argv, vec!["show", "abc", "--last", "3", "--json"]);
    }

    #[test]
    fn a_boolean_flag_is_present_only_when_true() {
        let on = build_argv(&show(), &json!({"id": "a", "last": 1, "subagents": true})).unwrap();
        assert!(on.contains(&"--subagents".to_string()), "{on:?}");
        for off in [
            json!({"id": "a", "last": 1, "subagents": false}),
            json!({"id": "a", "last": 1}),
        ] {
            let argv = build_argv(&show(), &off).unwrap();
            assert!(!argv.contains(&"--subagents".to_string()), "{argv:?}");
        }
    }

    #[test]
    fn positionals_lead_the_argv_in_declaration_order() {
        let argv = build_argv(
            &cat(),
            &json!({"tool_use_id": "toolu_9", "session": "abc", "input": true}),
        )
        .unwrap();
        // No `--json` on cat: the command has no JSON form, so the call returns text.
        assert_eq!(argv, vec!["cat", "abc", "toolu_9", "--input"]);
    }

    #[test]
    fn option_ids_become_kebab_case_long_flags() {
        let argv = build_argv(&show(), &json!({"id": "a", "max_bytes": 1024})).unwrap();
        assert_eq!(argv, vec!["show", "a", "--max-bytes", "1024", "--json"]);
    }

    #[test]
    fn integral_floats_are_accepted_because_clients_serialise_them() {
        let argv = build_argv(&show(), &json!({"id": "a", "last": 20.0})).unwrap();
        assert_eq!(argv, vec!["show", "a", "--last", "20", "--json"]);
    }

    #[test]
    fn a_missing_required_positional_is_an_invalid_param() {
        let e = build_argv(&cat(), &json!({"session": "abc"})).unwrap_err();
        assert!(e.downcast_ref::<crate::InvalidParams>().is_some(), "{e}");
        assert!(format!("{e}").contains("tool_use_id"), "{e}");
    }

    #[test]
    fn an_unknown_argument_is_rejected_rather_than_silently_dropped() {
        let e = build_argv(&show(), &json!({"id": "a", "limit": 5})).unwrap_err();
        assert!(e.downcast_ref::<crate::InvalidParams>().is_some(), "{e}");
        assert!(format!("{e}").contains("limit"), "{e}");
        // …but an echoed `json` is tolerated, since the wrapper supplies it anyway.
        assert!(build_argv(&show(), &json!({"id": "a", "last": 1, "json": true})).is_ok());
    }

    #[test]
    fn a_flag_given_a_non_boolean_is_an_invalid_param() {
        let e = build_argv(&show(), &json!({"id": "a", "last": 1, "subagents": "yes"})).unwrap_err();
        assert!(e.downcast_ref::<crate::InvalidParams>().is_some(), "{e}");
    }
}
