//! Retained UI content survives some native payload clearing. Project its known
//! semantics without treating a rendered command or exit code as a step status.
use super::{wire, File, Proto, Result, Stage, Write};

/// Emit a body only when the primary result was absent. Text stays in a private
/// arena so unsupported-only render records do not manufacture an empty result.
pub(super) fn content(stage: &Stage, info: &Proto, out: &mut impl Write) -> Result<bool> {
    let file = &stage.file;
    let mut present = false;
    let (at, len) = stage.append(|mut output| {
        body(file, info, &mut output, &mut present, 0)?;
        Ok(())
    })?;
    if present {
        out.write_all(b",\"content\":")?;
        crate::projection::write_json_string(out, wire::range(file, at, len)?)?;
    }
    Ok(present)
}

fn body(file: &File, info: &Proto, out: &mut impl Write, present: &mut bool, depth: usize) -> Result<()> {
    anyhow::ensure!(depth < 64, "native render nesting limit exceeded");
    if let Some(markdown) = info.last(2) {
        paragraph(file, markdown, out, present)?;
    }
    for field in info.repeated(10) {
        let block = field.message(file)?;
        // ContentBlock is a protobuf oneof; the last recognized member wins.
        let Some(field) = block.fields().iter().rev().find(|f| (1..=5).contains(&f.number)) else {
            continue;
        };
        match field.number {
            1 => paragraph(file, *field, out, present)?,
            3 => terminal(file, &field.message(file)?, out, present)?,
            4 => agent_text(file, &field.message(file)?, out, present)?,
            5 => {
                if let Some(nested) = field.message(file)?.last(1) {
                    body(file, &nested.message(file)?, out, present, depth + 1)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn nonempty(field: wire::Field) -> bool {
    matches!(field.value, wire::Value::Bytes { len, .. } if len > 0)
}

fn text(file: &File, field: wire::Field, out: &mut impl Write) -> Result<()> {
    let valid = crate::utf8::read(field.reader(file)?, crate::utf8::Verdict::Reject, |chunk| {
        out.write_all(chunk.as_bytes())
    })?;
    anyhow::ensure!(valid, "invalid native render UTF-8 string");
    Ok(())
}

fn separator(out: &mut impl Write, present: &mut bool) -> Result<()> {
    if *present {
        out.write_all(b"\n\n")?;
    }
    *present = true;
    Ok(())
}

fn paragraph(file: &File, field: wire::Field, out: &mut impl Write, present: &mut bool) -> Result<()> {
    if nonempty(field) {
        separator(out, present)?;
        text(file, field, out)?;
    }
    Ok(())
}

fn terminal(file: &File, block: &Proto, out: &mut impl Write, present: &mut bool) -> Result<()> {
    if ![1, 2, 4].into_iter().any(|n| block.last(n).is_some_and(nonempty)) && block.last(3).is_none() {
        return Ok(());
    }
    separator(out, present)?;
    if let Some(command) = block.last(1).filter(|f| nonempty(*f)) {
        out.write_all(b"$ ")?;
        text(file, command, out)?;
        out.write_all(b"\n")?;
    }
    if let Some(cwd) = block.last(2).filter(|f| nonempty(*f)) {
        out.write_all(b"[cwd: ")?;
        text(file, cwd, out)?;
        out.write_all(b"]\n")?;
    }
    if let Some(lines) = block.last(5).and_then(|f| f.u64()).filter(|n| *n > 0) {
        writeln!(out, "[{lines} earlier output lines omitted]")?;
    }
    if let Some(output) = block.last(4).filter(|f| nonempty(*f)) {
        text(file, output, out)?;
    }
    if let Some(code) = block.last(3).and_then(|f| f.u64()) {
        write!(out, "\n[exit code: {}]", code as i32)?;
    }
    Ok(())
}

fn agent_text(file: &File, agent: &Proto, out: &mut impl Write, present: &mut bool) -> Result<()> {
    if ![1, 2, 3].into_iter().any(|n| agent.last(n).is_some_and(nonempty)) {
        return Ok(());
    }
    separator(out, present)?;
    out.write_all(b"[Agent")?;
    if let Some(id) = agent.last(1).filter(|f| nonempty(*f)) {
        out.write_all(b" ")?;
        text(file, id, out)?;
    }
    out.write_all(b"]")?;
    if let Some(title) = agent.last(2).filter(|f| nonempty(*f)) {
        out.write_all(b" ")?;
        text(file, title, out)?;
    }
    if let Some(subtitle) = agent.last(3).filter(|f| nonempty(*f)) {
        out.write_all(b"\n")?;
        text(file, subtitle, out)?;
    }
    Ok(())
}

/// Keep explicit conversation identities usable as links independently of body
/// fallback. Repeated references retain ordering; consumers may deduplicate links.
pub(super) fn agents(file: &File, info: &Proto, out: &mut impl Write) -> Result<()> {
    out.write_all(b",\"native_agent_refs\":[")?;
    let mut first = true;
    agent_refs(file, info, out, &mut first, 0)?;
    out.write_all(b"]")?;
    Ok(())
}

fn agent_refs(file: &File, info: &Proto, out: &mut impl Write, first: &mut bool, depth: usize) -> Result<()> {
    anyhow::ensure!(depth < 64, "native render nesting limit exceeded");
    for (number, agent_number) in [(9, 5), (10, 4)] {
        for field in info.repeated(number) {
            let part = field.message(file)?;
            let selected = part.fields().iter().rev().find(|field| (1..=5).contains(&field.number));
            if let Some(agent) = selected.filter(|field| field.number == agent_number) {
                let agent = agent.message(file)?;
                if !*first {
                    out.write_all(b",")?;
                }
                *first = false;
                out.write_all(b"{")?;
                let mut first_field = true;
                for (number, name) in [(1, "conversation_id"), (2, "title"), (3, "subtitle")] {
                    if let Some(field) = agent.last(number) {
                        if !first_field {
                            out.write_all(b",")?;
                        }
                        first_field = false;
                        write!(out, "\"{name}\":")?;
                        field.write_json_string(file, out)?;
                    }
                }
                out.write_all(b"}")?;
            }
            if number == 10 {
                if let Some(nested) = selected
                    .filter(|field| field.number == 5)
                    .map(|f| f.message(file))
                    .transpose()?
                    .and_then(|s| s.last(1))
                {
                    agent_refs(file, &nested.message(file)?, out, first, depth + 1)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::harness::antigravity::cold::proto::field as bytes;

    fn project(data: &[u8]) -> (bool, serde_json::Value) {
        let stage = Stage::new().unwrap();
        let (at, len) = stage.append(|out| Ok(out.write_all(data)?)).unwrap();
        let info = Proto::read(&stage.file, at, len).unwrap();
        let mut output = b"{\"sentinel\":true".to_vec();
        let present = content(&stage, &info, &mut output).unwrap();
        agents(&stage.file, &info, &mut output).unwrap();
        output.extend_from_slice(b"}");
        (present, serde_json::from_slice(&output).unwrap())
    }

    #[test]
    fn retained_terminal_and_markdown_render_without_status_inference() {
        let terminal = [
            bytes(1, b"test"),
            bytes(2, b"/work"),
            vec![24, 2, 40, 7],
            bytes(4, b"failed\n"),
        ]
        .concat();
        let input = [bytes(10, &bytes(1, b"Before")), bytes(10, &bytes(3, &terminal))].concat();
        let (present, value) = project(&input);
        assert!(present);
        assert_eq!(
            value["content"],
            "Before\n\n$ test\n[cwd: /work]\n[7 earlier output lines omitted]\nfailed\n\n[exit code: 2]"
        );
        assert!(value.get("status").is_none());
    }

    #[test]
    fn agent_refs_preserve_identity_in_titles_and_nested_steps() {
        let agent = [bytes(1, b"child-id"), bytes(2, b"Research"), bytes(3, b"Working")].concat();
        let nested = bytes(10, &bytes(4, &agent));
        let info = [bytes(9, &bytes(5, &agent)), bytes(10, &bytes(5, &bytes(1, &nested)))].concat();
        let (present, value) = project(&info);
        assert!(present);
        assert_eq!(value["content"], "[Agent child-id] Research\nWorking");
        assert_eq!(value["native_agent_refs"].as_array().unwrap().len(), 2);
        assert_eq!(value["native_agent_refs"][1]["conversation_id"], "child-id");
    }

    #[test]
    fn empty_and_unsupported_render_info_does_not_invent_body() {
        for input in [Vec::new(), bytes(10, &bytes(2, &bytes(1, b"image-uri")))] {
            let (present, value) = project(&input);
            assert!(!present);
            assert!(value.get("content").is_none());
        }
    }
}
