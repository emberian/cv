//! Rendering an IR [`Session`] to human/LLM-friendly text. Shared by the `cv` CLI and `cv-mcp`.

use crate::ir::*;

/// Render a session as Markdown (good for sharing / feeding to another model).
pub fn to_markdown(s: &Session) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", s.label()));
    out.push_str(&format!("- harness: {}\n- id: {}\n", s.harness, s.id));
    if let Some(cwd) = &s.cwd {
        out.push_str(&format!("- cwd: {}\n", cwd.display()));
    }
    if let Some(m) = &s.model {
        out.push_str(&format!("- model: {m}\n"));
    }
    if let Some(t) = &s.created_at {
        out.push_str(&format!("- started: {}\n", t.to_rfc3339()));
    }
    out.push('\n');
    for m in &s.messages {
        let who = turn_label(m);
        out.push_str(&format!("## {who}\n\n"));
        for b in &m.content {
            render_block_md(b, &mut out);
        }
    }
    out
}

fn render_block_md(b: &Block, out: &mut String) {
    match b {
        Block::Text { text } => {
            out.push_str(text);
            out.push_str("\n\n");
        }
        Block::Thinking { text, .. } => {
            out.push_str("> 🧠 ");
            out.push_str(&text.replace('\n', "\n> "));
            out.push_str("\n\n");
        }
        Block::ToolUse { name, input, .. } => {
            out.push_str(&format!("**🔧 {name}**\n\n```json\n{input}\n```\n\n"));
        }
        Block::ToolResult { content, is_error, .. } => {
            out.push_str(&format!(
                "**↩ result{}**\n\n```\n{}\n```\n\n",
                if *is_error { " (error)" } else { "" },
                truncate(content, 4000)
            ));
        }
        Block::File { path, source, .. } => {
            out.push_str(&format!("_[file: {}]_\n\n", file_label(path, source)));
        }
        Block::Image { .. } => out.push_str("_[image]_\n\n"),
    }
}

fn file_label(path: &Option<String>, source: &Option<String>) -> String {
    path.as_deref().or(source.as_deref()).unwrap_or("?").to_string()
}

/// Compact plain-text rendering (terminal `show`).
pub fn to_plain(s: &Session, block_limit: usize) -> String {
    let mut out = String::new();
    for m in &s.messages {
        out.push_str(&format!("── {} ──\n", turn_label(m)));
        for b in &m.content {
            match b {
                Block::Text { text } => {
                    out.push_str(text);
                    out.push('\n');
                }
                Block::Thinking { text, .. } => out.push_str(&format!("[thinking] {}\n", truncate(text, block_limit))),
                Block::ToolUse { name, input, .. } => out.push_str(&format!(
                    "[tool_use {name}] {}\n",
                    truncate(&input.to_string(), block_limit)
                )),
                Block::ToolResult { content, is_error, .. } => out.push_str(&format!(
                    "[tool_result{}] {}\n",
                    if *is_error { " error" } else { "" },
                    truncate(content, block_limit)
                )),
                Block::File { path, source, .. } => out.push_str(&format!("[file: {}]\n", file_label(path, source))),
                Block::Image { .. } => out.push_str("[image]\n"),
            }
        }
        out.push('\n');
    }
    out
}

pub fn role_label(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// The label a rendered turn gets: the role, refined by its `kind` whenever the kind says more than
/// the role does (`system · compaction_boundary`, `user · compaction_summary`, `system · error`),
/// and for injected context by the harness's own attachment kind when it recorded one
/// (`system · injected_context/hook_success`). A plain prompt or reply is just `user` / `assistant`.
pub fn turn_label(m: &Message) -> String {
    let role = role_label(m.role);
    if m.kind == MessageKind::for_role(m.role) {
        return role.to_string();
    }
    let attachment_kind = m
        .extra
        .values()
        .filter_map(serde_json::Value::as_object)
        .find_map(|bag| bag.get("attachment_type"))
        .and_then(serde_json::Value::as_str);
    match (m.kind, attachment_kind) {
        (MessageKind::InjectedContext, Some(k)) => format!("{role} · {}/{k}", m.kind.as_str()),
        _ => format!("{role} · {}", m.kind.as_str()),
    }
}
