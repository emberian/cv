//! CLI-generated MCP tools (docs/INTERFACE-V2.md §6).
//!
//! The `cv` CLI is the single source of truth for what cv can do. Rather than hand-copying its
//! surface into `json!` literals — where names, flags, harness lists and help text drift the
//! moment the CLI moves — `cv mcp` builds one MCP tool per command from the **in-process** clap
//! tree, through the same `commands_json` that `cv schema --commands --json` prints. There is no
//! second binary to find, so the roster cannot skew from the CLI by construction.
//!
//! - **Which commands.** Everything clap shows under the **Read**, **Reshape**, **Export** and
//!   **System** groups, plus every `task` subcommand (`task_open`, `task_claim`, …). The rest of
//!   `Fleet & live` (`serve`, `watch`, `mcp`, `scry`, …) never returns, so it is no tool; the two
//!   that block on purpose (`await_omen`, `observe_stream`) are hand-written in `mcp.rs`.
//! - **Calls** run this same executable as `cv <command> <args…> --json` (the `--json` only for
//!   commands that have the flag) and return stdout. A non-zero exit becomes a tool error
//!   carrying stderr. (A subprocess, not a function call: every command prints to stdout.)
//! - **Windows.** `show` is the one command whose MCP defaults differ from the CLI's: with no
//!   window selector it gets `--last 50 --max-bytes 200000`, so a full transcript is never what an
//!   agent accidentally pulls into its context.
//!

use anyhow::{anyhow, Context as _, Result};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

use super::mcp::InvalidParams;

/// The command groups that become MCP tools. Of `Fleet & live`, only the one-shot `task`
/// subcommands do (see [`select_tool_commands`]).
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
    /// clap's argument id, also the MCP property name.
    pub name: String,
    #[serde(default)]
    pub long: Option<String>,
    #[serde(default)]
    pub multiple: bool,
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
    /// clap's actual spelling; older specs may only carry the argument id.
    pub fn long_flag(&self) -> String {
        format!("--{}", self.long.clone().unwrap_or_else(|| self.name.replace('_', "-")))
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

/// The generated half of the tool registry: this executable plus the commands it advertises.
pub(crate) struct CliTools {
    bin: PathBuf,
    commands: Vec<CommandSpec>,
}

impl CliTools {
    /// The commands of the in-process clap tree in the four tool groups, run through this
    /// executable.
    pub fn from_tree(cli: &clap::Command) -> Result<CliTools> {
        let all: Vec<CommandSpec> =
            serde_json::from_value(super::schema::commands_json(cli)).context("reading the clap tree")?;
        let commands = select_tool_commands(all);
        if commands.is_empty() {
            anyhow::bail!("the clap tree has no Read/Reshape/Export/System commands");
        }
        let bin = std::env::current_exe().context("locating this executable")?;
        Ok(CliTools { bin, commands })
    }

    /// Extract startup specs and release the command tree before serving requests.
    pub fn from_owned_tree(tree: clap::Command) -> Result<CliTools> {
        Self::from_tree(&tree)
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

    /// Run the generated CLI unchanged, retaining operation-owned bounded output.
    pub fn call_cancelled(
        &self,
        tool: &str,
        args: &Value,
        cancelled: super::mcp::Cancellation<'_>,
    ) -> Result<CommandOutput> {
        let cmd = self
            .find(tool)
            .ok_or_else(|| anyhow!("unknown generated tool: {tool}"))?;
        let argv = build_argv(cmd, args)?;
        run_command_cancelled(&self.bin, &argv, cancelled)
            .with_context(|| format!("running {} {}", self.bin.display(), argv.join(" ")))
    }
}

pub(crate) struct CapturedText {
    pub bytes: cv_core::bytes::ByteSource,
    pub len: u64,
    pub trimmed: std::ops::Range<u64>,
}
impl CapturedText {
    pub fn capture(input: impl std::io::Read) -> Result<Self> {
        use std::io::Write;
        let mut stage = cv_core::bytes::ByteStage::default();
        let mut len = 0u64;
        let mut first = None;
        let mut end = 0;
        cv_core::projection::for_each_lossy_utf8_chunk(input, |text| {
            let head = text.trim_start();
            if !head.is_empty() {
                first.get_or_insert(len + (text.len() - head.len()) as u64);
                end = len + text.trim_end().len() as u64;
            }
            stage.write_all(text.as_bytes())?;
            len += text.len() as u64;
            Ok(())
        })?;
        Ok(Self {
            bytes: stage.finish()?,
            len,
            trimmed: first.unwrap_or(0)..end,
        })
    }
    pub fn reader(&self, trimmed: bool) -> std::io::Result<Box<dyn std::io::Read + '_>> {
        use std::io::Read;
        let mut reader = self.bytes.reader()?;
        let range = if trimmed { self.trimmed.clone() } else { 0..self.len };
        std::io::copy(&mut reader.by_ref().take(range.start), &mut std::io::sink())?;
        Ok(Box::new(reader.take(range.end - range.start)))
    }
}
pub(crate) struct CommandOutput {
    pub stdout: CapturedText,
    pub stderr: CapturedText,
    pub error_prefix: Option<String>,
}
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        // Only this invocation's child is owned here; kill/reap on error, panic or disconnect.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[cfg(test)]
pub(super) fn run_command(
    bin: &std::path::Path,
    argv: &[String],
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<CommandOutput> {
    run_command_cancelled(bin, argv, super::mcp::Cancellation::connection(cancelled))
}

fn run_command_cancelled(
    bin: &std::path::Path,
    argv: &[String],
    cancelled: super::mcp::Cancellation<'_>,
) -> Result<CommandOutput> {
    use std::process::Stdio;
    cancelled.check()?;
    std::thread::scope(|scope| {
        let mut child = Child(
            cv_core::env::command(bin)
                .args(argv)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
        let stdout = child.0.stdout.take().unwrap();
        let stderr = child.0.stderr.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let other = tx.clone();
        let out = scope.spawn(move || {
            let _ = tx.send((
                true,
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| CapturedText::capture(stdout))),
            ));
        });
        let err = scope.spawn(move || {
            let _ = other.send((
                false,
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| CapturedText::capture(stderr))),
            ));
        });
        let mut stdout = None;
        let mut stderr = None;
        let mut failure = None;
        let mut collector_panic = None;
        while stdout.is_none() || stderr.is_none() {
            if cancelled.is_cancelled() {
                let _ = child.0.kill();
                failure.get_or_insert_with(|| cancelled.check().unwrap_err());
            }
            match rx.recv_timeout(std::time::Duration::from_millis(10)) {
                Ok((is_stdout, result)) => {
                    let result = match result {
                        Ok(result) => result,
                        Err(panic) => {
                            collector_panic = Some(panic);
                            Err(anyhow!("output collector panicked"))
                        }
                    };
                    if result.is_err() {
                        let _ = child.0.kill();
                    }
                    if is_stdout {
                        stdout = Some(result);
                    } else {
                        stderr = Some(result);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        for thread in [out, err] {
            if let Err(panic) = thread.join() {
                std::panic::resume_unwind(panic);
            }
        }
        if let Some(panic) = collector_panic {
            std::panic::resume_unwind(panic);
        }
        if let Some(error) = failure {
            return Err(error);
        }
        let stdout = stdout.context("stdout collector stopped")??;
        let stderr = stderr.context("stderr collector stopped")??;
        let status = loop {
            cancelled.check()?;
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let error_prefix = (!status.success()).then(|| {
            let code = status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".into());
            format!("error: cv {} exited {code}: ", argv.join(" "))
        });
        Ok(CommandOutput {
            stdout,
            stderr,
            error_prefix,
        })
    })
}

/// Keep the commands in the four tool groups and the `task` subcommands, dropping bare dispatcher
/// parents (a command that has subcommands and no arguments of its own — calling it could only
/// ever print clap's usage) and `hook`, which reads Claude Code's hook JSON on stdin that an MCP
/// call cannot supply (it did nothing; `cv hooks install claude` wires it). A `--follow` flag is
/// dropped too: a tool call has no Ctrl-C, so following is offered only bounded, through `for`.
fn select_tool_commands(all: Vec<CommandSpec>) -> Vec<CommandSpec> {
    let names: Vec<String> = all.iter().map(|c| c.name.clone()).collect();
    all.into_iter()
        .filter(|c| (TOOL_GROUPS.contains(&c.group.as_str()) && c.name != "hook") || c.name.starts_with("task "))
        .filter(|c| {
            let has_children = names.iter().any(|n| n.starts_with(&format!("{} ", c.name)));
            !(has_children && c.args.is_empty())
        })
        .map(|mut c| {
            c.args.retain(|a| !(a.name == "follow" && a.kind == ArgKind::Flag));
            c
        })
        .collect()
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
             (nor `kind`) given it reads the LAST {SHOW_DEFAULT_LAST} messages capped at \
             {SHOW_DEFAULT_MAX_BYTES} bytes, so a whole transcript is never pulled in by accident. \
             Ask for a window explicitly to override."
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
    if a.multiple {
        // Advertised as a typed array (hosts reject untyped properties); a bare value is
        // still accepted on input and repeats the flag once.
        let description = p.remove("description").unwrap();
        p.remove("default");
        json!({"description": description, "type": "array", "items": p})
    } else {
        Value::Object(p)
    }
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
    let empty = Map::new();
    let obj = match args {
        Value::Null => &empty,
        Value::Object(o) => o,
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
    if let Some(a) = cmd.args.iter().find(|a| a.required && present(obj, &a.name).is_none()) {
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
        match present(obj, &a.name) {
            // A list fills a repeatable positional (`splice <spec>…`) in order.
            Some(Value::Array(items)) => {
                for v in items {
                    argv.push(scalar(&a.name, v)?);
                }
            }
            Some(v) => argv.push(scalar(&a.name, v)?),
            // Required ones were caught above; a hole in the OPTIONAL tail (`workflow <id>
            // [<run_id>]`) just ends the run — skipping it would shift later values into the
            // wrong slot.
            None => break,
        }
    }

    for a in cmd.args.iter().filter(|a| a.kind == ArgKind::Option) {
        let Some(v) = present(obj, &a.name) else { continue };
        // A list repeats the flag (`--blocked-by A --blocked-by B`), as a repeatable option takes it.
        let values = match v {
            Value::Array(items) => items.iter().collect(),
            v => vec![v],
        };
        for v in values {
            argv.push(a.long_flag());
            argv.push(scalar(&a.name, v)?);
        }
    }

    for a in cmd.args.iter().filter(|a| a.kind == ArgKind::Flag) {
        if a.name == "json" {
            continue;
        }
        match present(obj, &a.name) {
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

    apply_mcp_defaults(cmd, obj, &mut argv);

    if cmd.has_json() {
        argv.push("--json".into());
    }
    Ok(argv)
}

/// `show` without a window selector reads the tail, not the whole transcript (§6). A `kind` view
/// (what the person said, the compaction seams) is rows, not a transcript: it gets no default.
fn apply_mcp_defaults(cmd: &CommandSpec, obj: &Map<String, Value>, argv: &mut Vec<String>) {
    if cmd.tool_name() != "show" {
        return;
    }
    let windowed = WINDOW_SELECTORS
        .iter()
        .any(|k| cmd.arg(k).is_some() && present(obj, k).is_some());
    if windowed || present(obj, "kind").is_some() {
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
    InvalidParams(msg).into()
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
              {"name":"pre_compaction","kind":"option","value_type":"integer","help":"Pre-compaction span","required":false},
              {"name":"kind","kind":"option","value_type":"string","help":"Only these kinds","required":false}
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
    fn hook_is_no_tool_but_its_installer_is() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024) // a debug clap tree overflows the 2 MB test stack
            .spawn(|| {
                let tools = CliTools::from_tree(&crate::build_cli()).unwrap();
                assert!(tools.find("hook").is_none());
                assert!(tools.find("hooks_install").is_some());
            })
            .unwrap()
            .join()
            .unwrap();
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
        assert_eq!(
            names,
            vec!["formats_census"],
            "dispatchers and the rest of Fleet & live are dropped"
        );

        let all: Vec<CommandSpec> = serde_json::from_str(
            r#"[{"name":"task","group":"Fleet & live","about":"Tasks","args":[]},
                {"name":"task claim","group":"Fleet & live","about":"Claim","args":[
                   {"name":"id","kind":"positional","value_type":"string","help":"Task","required":true}]},
                {"name":"serve","group":"Fleet & live","about":"Serve","args":[
                   {"name":"port","kind":"option","value_type":"integer","help":"p","required":false}]}]"#,
        )
        .unwrap();
        let names: Vec<String> = select_tool_commands(all).iter().map(|c| c.tool_name()).collect();
        assert_eq!(names, vec!["task_claim"], "task subcommands are tools; a server is not");
    }

    // -- argv construction -------------------------------------------------

    #[test]
    fn generated_task_flags_and_repeatable_arguments_round_trip_through_clap() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let tree = crate::build_cli();
                let tools = CliTools::from_tree(&tree).unwrap();
                for (name, args, expected) in [
                    (
                        "task_block",
                        json!({"id":"a", "by_task":"b"}),
                        vec!["task", "block", "a", "--by", "b"],
                    ),
                    (
                        "task_supersede",
                        json!({"id":"a", "by_task":"b"}),
                        vec!["task", "supersede", "a", "--by", "b"],
                    ),
                    (
                        "task_decide",
                        json!({"title":"question", "for_who":"ember", "default":"a", "options":["b", "c"]}),
                        vec![
                            "task",
                            "decide",
                            "question",
                            "--for",
                            "ember",
                            "--default",
                            "a",
                            "--option",
                            "b",
                            "--option",
                            "c",
                        ],
                    ),
                    (
                        "splice",
                        json!({"specs":["a:..2", "b:2.."]}),
                        vec!["splice", "a:..2", "b:2..", "--json"],
                    ),
                ] {
                    let cmd = tools.find(name).unwrap();
                    let argv = build_argv(cmd, &args).unwrap();
                    assert_eq!(argv, expected);
                    tree.clone()
                        .try_get_matches_from(std::iter::once("cv".to_owned()).chain(argv))
                        .unwrap();
                }
                for (tool, arg) in [
                    ("task_decide", "options"),
                    ("task_open", "blocked_by"),
                    ("splice", "specs"),
                    ("adopt", "agents"),
                    ("distill", "with"),
                ] {
                    let descriptor = tool_descriptor(tools.find(tool).unwrap());
                    let property = &descriptor["inputSchema"]["properties"][arg];
                    assert_eq!(property["type"], "array");
                    assert_eq!(property["items"]["type"], "string");
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

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
    fn a_kind_view_gets_no_window_default() {
        let argv = build_argv(&show(), &json!({"id": "abc", "kind": "prompt,answer"})).unwrap();
        assert_eq!(argv, vec!["show", "abc", "--kind", "prompt,answer", "--json"]);
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
    fn a_list_repeats_an_option_and_fills_a_positional() {
        let c = spec(
            r#"{"name":"task open","group":"Fleet & live","about":"Open","args":[
              {"name":"title","kind":"positional","value_type":"string","help":"t","required":true},
              {"name":"blocked_by","kind":"option","value_type":"string","help":"b","required":false}]}"#,
        );
        let argv = build_argv(&c, &json!({"title": "x", "blocked_by": ["a", "b"]})).unwrap();
        assert_eq!(
            argv,
            vec!["task", "open", "x", "--blocked-by", "a", "--blocked-by", "b"]
        );
        let argv = build_argv(&cat(), &json!({"session": ["s"], "tool_use_id": "t"})).unwrap();
        assert_eq!(argv, vec!["cat", "s", "t"]);
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
        assert!(e.downcast_ref::<InvalidParams>().is_some(), "{e}");
        assert!(format!("{e}").contains("tool_use_id"), "{e}");
    }

    #[test]
    fn an_unknown_argument_is_rejected_rather_than_silently_dropped() {
        let e = build_argv(&show(), &json!({"id": "a", "limit": 5})).unwrap_err();
        assert!(e.downcast_ref::<InvalidParams>().is_some(), "{e}");
        assert!(format!("{e}").contains("limit"), "{e}");
        // …but an echoed `json` is tolerated, since the wrapper supplies it anyway.
        assert!(build_argv(&show(), &json!({"id": "a", "last": 1, "json": true})).is_ok());
    }

    #[test]
    fn a_flag_given_a_non_boolean_is_an_invalid_param() {
        let e = build_argv(&show(), &json!({"id": "a", "last": 1, "subagents": "yes"})).unwrap_err();
        assert!(e.downcast_ref::<InvalidParams>().is_some(), "{e}");
    }
}

#[cfg(all(test, unix))]
mod transport_tests {
    use super::*;
    #[test]
    fn staged_child_drains_both_pipes_and_preserves_exit_fallback() {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let output = run_command(
            std::path::Path::new("/bin/sh"),
            &[
                "-c".into(),
                "head -c 2097152 /dev/zero >&2; head -c 2097152 /dev/zero; exit 3".into(),
            ],
            &cancel,
        )
        .unwrap();
        assert_eq!(output.stdout.len, 2097152);
        assert_eq!(output.stderr.len, 2097152);
        assert!(output.error_prefix.as_ref().unwrap().ends_with("exited 3: "));
        let white = CapturedText::capture(" \n\u{2003}".as_bytes()).unwrap();
        assert!(white.trimmed.is_empty());
        let raw = [b' ', 0xff, b' ', b'\n'];
        let text = CapturedText::capture(raw.as_slice()).unwrap();
        let mut actual = String::new();
        std::io::Read::read_to_string(&mut text.reader(true).unwrap(), &mut actual).unwrap();
        assert_eq!(actual, "\u{fffd}");
    }
    #[test]
    fn staged_cancelled_child_is_reaped() {
        let dir = std::env::temp_dir().join(format!("cv-mcp-child-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let pid_path = dir.join("pid");
        // Publish the pid by rename: the file exists only once it holds the whole pid, so the
        // canceller below cannot fire between the shell creating it and writing to it.
        let quote = |p: &std::path::Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
        let script = format!(
            "echo $$ > {staged} && mv {staged} {pid}; exec sleep 10",
            staged = quote(&dir.join("pid.staged")),
            pid = quote(&pid_path),
        );

        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let connection = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                // A loaded machine can take a while to start the shell; cancel only once it runs.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                while !pid_path.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            let error = run_command_cancelled(
                std::path::Path::new("/bin/sh"),
                &["-c".into(), script],
                super::super::mcp::Cancellation::request(&connection, &cancelled),
            )
            .err()
            .expect("cancelled child must fail");
            assert!(error.to_string().contains("MCP request cancelled"), "{error:#}");
            assert!(!connection.load(std::sync::atomic::Ordering::Relaxed));
        });
        let pid: i32 = std::fs::read_to_string(&pid_path).unwrap().trim().parse().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
