//! Rendering an IR [`Session`] to human/LLM-friendly text. Shared by the `cv` CLI and `cv mcp`.

use crate::ir::*;
#[cfg(test)]
use crate::json::{ReadInput, Validation};
use chrono::{DateTime, Utc};
use std::io::Write;
use std::path::Path;

/// Write complete Markdown with bounded scratch for lazy text and JSON leaves. Callers must
/// discard staged output on error: a source or writer failure may follow an emitted prefix.
pub fn write_markdown(s: &Session, out: &mut impl Write, resolver: &crate::Resolver) -> anyhow::Result<()> {
    let reads = crate::source::session_reads(s);
    let label = label_from(s.title_with(resolver)?.as_deref(), None);
    out.write_all(
        md_header(
            &label,
            s.harness,
            &s.id,
            s.cwd.as_deref(),
            s.model.as_deref(),
            s.created_at,
        )
        .as_bytes(),
    )?;
    if let Some(coverage) = crate::coverage::ReadCoverage::of_session(s) {
        writeln!(out, "Read coverage: {}\n", coverage.summary())?;
    }
    for message in &s.messages {
        write_md_message(message, out, resolver)?;
    }
    anyhow::ensure!(
        reads.fresh(&mut crate::source::Probe::default()),
        "session sources changed during Markdown output"
    );
    Ok(())
}

/// Exact character-bounded preview of concatenated plain-text blocks, including whitespace-only
/// text. Checked demands scan discarded tails without retaining them.
pub fn message_text_preview(m: &Message, resolver: &crate::Resolver, chars: usize) -> anyhow::Result<Option<String>> {
    let mut head = crate::projection::Head::bounded(chars);
    let mut nonempty = false;
    for block in &m.content {
        if let Block::Text { text } = block {
            if nonempty {
                head.push("\n");
            }
            text.try_for_each_chunk(resolver, 64 * 1024, |part| {
                nonempty |= !part.is_empty();
                head.push(part);
                Ok(())
            })?;
        }
    }
    Ok(nonempty.then(|| head.finish()))
}

/// Write one Markdown section, preserving the owned renderer's exact formatting.
pub fn write_md_message(m: &Message, out: &mut impl Write, resolver: &crate::Resolver) -> anyhow::Result<()> {
    write!(out, "## {}\n\n", turn_label(m))?;
    for block in &m.content {
        write_md_block(block, out, resolver)?;
    }
    Ok(())
}

/// Write a Markdown block. Result excerpts retain only 4001 decoded scalars, while the entire
/// source is demanded so discarded tails cannot hide read errors or stale dependencies.
pub fn write_md_block(b: &Block, out: &mut impl Write, resolver: &crate::Resolver) -> anyhow::Result<()> {
    match b {
        Block::Text { text } => {
            text.try_for_each_chunk(resolver, 64 * 1024, |part| {
                out.write_all(part.as_bytes())?;
                Ok(())
            })?;
            out.write_all(b"\n\n")?;
        }
        Block::Thinking { text, .. } => {
            out.write_all("> 🧠 ".as_bytes())?;
            text.try_for_each_chunk(resolver, 64 * 1024, |part| {
                for (i, line) in part.split('\n').enumerate() {
                    if i != 0 {
                        out.write_all(b"\n> ")?;
                    }
                    out.write_all(line.as_bytes())?;
                }
                Ok(())
            })?;
            out.write_all(b"\n\n")?;
        }
        Block::ToolUse { name, input, .. } => {
            write!(out, "**🔧 {name}**\n\n```json\n")?;
            input.write_canonical(out)?;
            out.write_all(b"\n```\n\n")?;
        }
        Block::ToolResult { content, is_error, .. } => {
            let mut head = crate::projection::Head::bounded(4000);
            content.try_for_each_chunk(resolver, 64 * 1024, |part| {
                head.push(part);
                Ok(())
            })?;
            // Tool output is a literal code excerpt, so preserve its line structure (the kept
            // text, not `finish`'s flattening); the lookahead tells an exact fit from a cut.
            let mut excerpt = head.as_str().to_owned();
            if head.full() {
                excerpt.pop();
                excerpt.pop();
                excerpt.push('…');
            }
            // A tool can return Markdown containing its own fenced blocks. Keep that text
            // literal by choosing a fence longer than every run in the rendered excerpt.
            let longest = excerpt.split(|c| c != '`').map(str::len).max().unwrap_or(0);
            let fence = "`".repeat(3.max(longest + 1));
            write!(
                out,
                "**↩ result{}**\n\n{fence}\n{}\n{fence}\n\n",
                if *is_error { " (error)" } else { "" },
                excerpt
            )?;
        }
        Block::File { path, source, .. } => write!(out, "_[file: {}]_\n\n", file_label(path, source))?,
        Block::Image { .. } => out.write_all(b"_[image]_\n\n")?,
    }
    Ok(())
}

/// Render a session as Markdown (good for sharing / feeding to another model). The streaming
/// `cv export --format md` renders the same header and sections one message at a time through
/// [`md_header`] and [`md_message`].
pub fn to_markdown(s: &Session) -> anyhow::Result<String> {
    let mut out = Vec::new();
    write_markdown(s, &mut out, &s.resolver())?;
    Ok(String::from_utf8(out)?)
}

/// The Markdown front-matter: title, harness, id, cwd (home-relative), model and start time, each
/// only when known, then a blank line.
pub fn md_header(
    label: &str,
    harness: Harness,
    id: &str,
    cwd: Option<&Path>,
    model: Option<&str>,
    started: Option<DateTime<Utc>>,
) -> String {
    let mut out = format!("# {label}\n\n- harness: {harness}\n- id: {id}\n");
    if let Some(cwd) = cwd {
        out.push_str(&format!("- cwd: {}\n", home_rel(cwd)));
    }
    if let Some(m) = model {
        out.push_str(&format!("- model: {m}\n"));
    }
    if let Some(t) = started {
        out.push_str(&format!("- started: {}\n", t.to_rfc3339()));
    }
    out.push('\n');
    out
}

fn file_label(path: &Option<String>, source: &Option<String>) -> String {
    path.as_deref().or(source.as_deref()).unwrap_or("?").to_string()
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
/// A turn cv inferred rather than read ([`Message::synthesized`]) says so:
/// `system · compaction_boundary · synthesized`.
pub fn turn_label(m: &Message) -> String {
    let role = role_label(m.role);
    let label = if m.kind == MessageKind::for_role(m.role) {
        role.to_string()
    } else {
        match (m.kind, crate::harness::attachment_kind(m)) {
            (MessageKind::InjectedContext, Some(k)) => format!("{role} · {}/{k}", m.kind.as_str()),
            _ => format!("{role} · {}", m.kind.as_str()),
        }
    };
    if m.is_synthesized() {
        format!("{label} · synthesized")
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Source(std::path::PathBuf);
    impl Source {
        fn new(bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!("cv-render-{}", uuid::Uuid::new_v4()));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
        fn json(&self) -> crate::Json {
            crate::Json::source(&self.0, 0, std::fs::metadata(&self.0).unwrap().len()).unwrap()
        }
    }
    impl Drop for Source {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn source_text(value: &crate::Json, key: &str) -> crate::Text {
        value
            .select(key)
            .unwrap()
            .unwrap()
            .with_view(|view| Ok(view.source_text()?.unwrap()))
            .unwrap()
    }
    #[test]
    fn markdown_title_skips_empty_user_turns_and_propagates_candidate_errors() -> anyhow::Result<()> {
        let mut session = Session::new("title", Harness::Codex);
        for text in ["   ", "actual title"] {
            let mut message = Message::new(Role::User);
            message.content.push(Block::Text { text: text.into() });
            session.messages.push(message);
        }
        assert!(to_markdown(&session)?.starts_with("# actual title\n"));
        let world = crate::test_world("markdown-title-source");
        let path = world.base.join("title.json");
        std::fs::write(&path, serde_json::to_vec(&"lazy title".repeat(2000))?)?;
        let source = crate::json::JsonSource::capture(&path)?;
        let text = source
            .read(ReadInput::Range(0, source.len()), Validation::Value, |view| {
                view.source_text()
            })?
            .unwrap();
        session.messages[1].content = vec![Block::Text { text }];
        std::fs::write(&path, b"changed title source")?;
        assert!(to_markdown(&session).is_err());
        Ok(())
    }

    #[test]
    fn markdown_tool_results_keep_nested_fences_literal() {
        let mut output = Vec::new();
        let block = Block::ToolResult {
            tool_use_id: "tool".into(),
            content: "before\n```rust\nlet x = 1;\n```\nafter".into(),
            is_error: false,
            tool_name: None,
            status: None,
            details: None,
        };
        write_md_block(&block, &mut output, &crate::Resolver::new(None)).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "**↩ result**\n\n````\nbefore\n```rust\nlet x = 1;\n```\nafter\n````\n\n"
        );
    }

    #[test]
    fn markdown_result_excerpt_preserves_newlines_and_scalar_limit() {
        for length in [3999, 4000, 4001] {
            let body = format!("line\n{}", "α".repeat(length - 5));
            let block = Block::ToolResult {
                tool_use_id: "tool".into(),
                content: body.clone().into(),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
            };
            let mut output = Vec::new();
            write_md_block(&block, &mut output, &crate::Resolver::new(None)).unwrap();
            let output = String::from_utf8(output).unwrap();
            let excerpt = output
                .strip_prefix("**↩ result**\n\n```\n")
                .unwrap()
                .strip_suffix("\n```\n\n")
                .unwrap();
            assert!(excerpt.starts_with("line\n"));
            assert_eq!(excerpt.chars().count(), length.min(4000));
            if length > 4000 {
                assert!(excerpt.ends_with('…'));
            } else {
                assert_eq!(excerpt, body);
            }
        }
    }

    #[test]
    fn checked_session_writers_match_complete_owned_format_oracles() {
        let fixture = serde_json::json!({
            "text": "α\n🌙\\\"".repeat(20000),
            "result": "🌙\n".repeat(3000),
            "input": {"nested": [[], {}, "α\n\\\"", 1.25]}
        });
        let source = Source::new(&serde_json::to_vec(&fixture).unwrap());
        let value = source.json();
        let mut session = Session::new("checked", Harness::Codex);
        session.title = Some("complete formatting".into());
        session.model = Some("model".into());
        session.cwd = Some("/tmp/fixture".into());
        session.system_prompt = Some("system\n🌙".into());
        session
            .extra
            .insert("cv".into(), serde_json::json!({"loom": [{"source_id": "one"}]}));
        let mut message = Message::new(Role::User);
        message.content = vec![
            Block::Text {
                text: source_text(&value, "text"),
            },
            Block::Thinking {
                text: source_text(&value, "text"),
                signature: None,
                encrypted: None,
                redacted: false,
            },
            Block::ToolUse {
                id: "call".into(),
                name: "tool".into(),
                namespace: None,
                input: value.select("input").unwrap().unwrap(),
            },
            Block::ToolResult {
                tool_use_id: "call".into(),
                content: source_text(&value, "result"),
                is_error: true,
                tool_name: None,
                status: None,
                details: None,
            },
            Block::File {
                mime: None,
                path: None,
                source: Some("asset".into()),
            },
            Block::Image {
                media_type: None,
                data_ref: None,
            },
        ];
        session.messages.push(message);
        session.messages.push(Message::new(Role::Assistant));
        let mut owned = session.clone();
        owned.materialize().unwrap();
        let resolver = session.resolver();
        let mut markdown = Vec::new();
        write_markdown(&session, &mut markdown, &resolver).unwrap();
        assert_eq!(to_markdown(&session).unwrap().as_bytes(), markdown);
        let mut frozen = crate::emit::frozen::Frozen::new("render-markdown");
        frozen.insert(
            "titled",
            serde_json::json!([markdown.len(), crate::digest::sha256_hex(&markdown)]),
        );
        session.title = None;
        owned.title = None;
        markdown.clear();
        write_markdown(&session, &mut markdown, &resolver).unwrap();
        frozen.insert(
            "untitled",
            serde_json::json!([markdown.len(), crate::digest::sha256_hex(&markdown)]),
        );
        frozen.check().unwrap();
        let mut compact = Vec::new();
        session.write_json(&mut compact, &resolver).unwrap();
        assert_eq!(compact, serde_json::to_vec(&owned).unwrap());
        let mut pretty = Vec::new();
        session.write_json_pretty(&mut pretty, &resolver).unwrap();
        assert_eq!(pretty, serde_json::to_vec_pretty(&owned).unwrap());
        let empty = Session::new("empty", Harness::Claude);
        pretty.clear();
        empty.write_json_pretty(&mut pretty, &empty.resolver()).unwrap();
        assert_eq!(pretty, serde_json::to_vec_pretty(&empty).unwrap());
    }

    #[test]
    fn checked_message_previews_match_join_and_truncate_oracle() {
        for pieces in [
            vec!["", ""],
            vec!["\u{2003}\n", "\t"],
            vec!["", "alpha", ""],
            vec![" \n", "🌙", "tail"],
        ] {
            let mut m = Message::new(Role::User);
            for text in pieces {
                m.content.push(Block::Text { text: text.into() });
            }
            assert_eq!(
                message_text_preview(&m, &crate::Resolver::new(None), 80).unwrap(),
                m.text().map(|t| truncate(&t, 80))
            );
        }
    }

    #[test]
    fn staged_session_outputs_reject_a_source_changed_after_its_last_leaf() {
        struct Mutating<'a> {
            stage: crate::bytes::ByteStage,
            path: &'a Path,
            changed: bool,
        }
        impl Write for Mutating<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                // The later message's role is emitted after all first-message lazy demands.
                if !self.changed && bytes.windows(9).any(|part| part == b"assistant") {
                    std::fs::remove_file(self.path)?;
                    self.changed = true;
                }
                self.stage.write(bytes)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.stage.flush()
            }
        }
        for json in [false, true] {
            let source = Source::new(&serde_json::to_vec(&serde_json::json!({"text":"early".repeat(1000)})).unwrap());
            let mut session = Session::new("guarded", Harness::Codex);
            let mut first = Message::new(Role::User);
            first.content.push(Block::Text {
                text: source_text(&source.json(), "text"),
            });
            assert!(matches!(&first.content[0], Block::Text { text } if text.is_lazy()));
            session.messages = vec![first, Message::new(Role::Assistant)];
            let mut writer = Mutating {
                stage: crate::bytes::ByteStage::new(),
                path: &source.0,
                changed: false,
            };
            let result = if json {
                session.write_json_pretty(&mut writer, &session.resolver())
            } else {
                write_markdown(&session, &mut writer, &session.resolver())
            };
            assert!(writer.changed);
            assert!(result.is_err());
            assert!(!writer.stage.is_empty());
            // No finished ByteSource exists for publication after an error.
        }
    }
}
