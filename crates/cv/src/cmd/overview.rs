//! Compact, source-linked overview shared with the read API.

use anyhow::Result;
use cv_core::sanitize::sanitize_line;
use cv_core::SessionRef;
use serde_json::Value;

pub(crate) fn print(r: &SessionRef, json: bool) -> Result<()> {
    let overview = cv_core::overview::read(r, &Default::default())?;
    let value = serde_json::to_value(overview)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print!("{}", render(&value));
    }
    Ok(())
}

fn text(value: &Value, key: &str) -> String {
    sanitize_line(value.get(key).and_then(Value::as_str).unwrap_or("")).into_owned()
}

fn excerpt(out: &mut String, row: &Value, key: &str) {
    for line in row.get(key).and_then(Value::as_str).unwrap_or("").lines() {
        out.push_str("    ");
        out.push_str(&sanitize_line(line));
        out.push('\n');
    }
    if row.get("truncated").and_then(Value::as_bool) == Some(true) {
        out.push_str("    … open the source for the rest\n");
    }
}

pub(crate) fn render(value: &Value) -> String {
    let session = &value["session"];
    let selector = format!("{}:{}", text(session, "harness"), text(session, "id"));
    let path = text(session, "path");
    let source_selector = if path.is_empty() {
        selector.clone()
    } else {
        format!("{}:{path}", text(session, "harness"))
    };
    let quoted_selector = cv_core::harness::shell_quote(&source_selector);
    let mut out = format!("{}\n{}\n", text(session, "title"), selector);
    for (field, heading) in [
        ("requests", "Requests"),
        ("workers", "Workers"),
        ("files", "Referenced files"),
        ("commands", "Recent commands"),
    ] {
        out.push_str(&format!("\n{heading}\n"));
        let rows = value[field].as_array().map(Vec::as_slice).unwrap_or_default();
        if rows.is_empty() {
            out.push_str(if field == "workers" {
                "  No worker records found; discovery limits, if any, are listed below.\n"
            } else {
                "  None recorded.\n"
            });
        }
        for row in rows {
            let index = row.get("msg_idx").and_then(Value::as_u64);
            match field {
                "requests" => {
                    out.push_str(&format!(
                        "  [{}]\n",
                        index.map_or_else(|| "?".into(), |i| i.to_string())
                    ));
                    excerpt(&mut out, row, "text");
                }
                "workers" => {
                    out.push_str(&format!("  {} · {}\n", text(row, "agent_id"), text(row, "status")));
                    if let Some(description) = row.get("description").and_then(Value::as_str) {
                        out.push_str(&format!("    {}\n", sanitize_line(description)));
                    }
                    excerpt(&mut out, row, "last_text");
                    out.push_str(&format!(
                        "    cv show {quoted_selector} --agent {}\n",
                        cv_core::harness::shell_quote(&text(row, "agent_id"))
                    ));
                }
                "files" => {
                    let missing = if row.get("present").and_then(Value::as_bool) == Some(false) {
                        " (missing)"
                    } else {
                        ""
                    };
                    out.push_str(&format!(
                        "  [{}] {} · {}{missing}\n",
                        index.map_or_else(|| "?".into(), |i| i.to_string()),
                        text(row, "kind"),
                        text(row, "path")
                    ));
                }
                "commands" => {
                    out.push_str(&format!(
                        "  [{}]\n",
                        index.map_or_else(|| "?".into(), |i| i.to_string())
                    ));
                    if row.get("command").is_some_and(Value::is_string) {
                        excerpt(&mut out, row, "command");
                    } else {
                        out.push_str("    Dynamic command; open the tool call for its arguments.\n");
                    }
                }
                _ => unreachable!(),
            }
        }
        if let Some(n) = value["omitted"][field].as_u64().filter(|n| *n > 0) {
            out.push_str(&format!("  {n} more omitted from this overview.\n"));
        }
    }
    if let Some(warnings) = value["warnings"].as_array() {
        for warning in warnings.iter().filter_map(Value::as_str) {
            out.push_str(&format!("\nNote: {}\n", sanitize_line(warning)));
        }
    }
    if let Ok(coverage) = serde_json::from_value::<cv_core::coverage::ReadCoverage>(value["read_coverage"].clone()) {
        out.push_str(&format!("\n{}\n", coverage.summary()));
    }
    out.push_str(&format!(
        "\nRead surrounding messages: cv show {quoted_selector} --around N\n"
    ));
    out
}
