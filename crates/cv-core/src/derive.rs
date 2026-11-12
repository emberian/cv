//! Lossless native-record derivation: selectors choose checked source ranges and replacement
//! nodes; one writer applies identity/standalone changes and preserves every untouched byte.
use crate::harness::store::jsonl::ranges::{self, Range};
use crate::harness::Identity;
use crate::ir::Harness;
use crate::json::{Json, JsonSource};
use crate::json::{ReadInput, Validation};
use anyhow::Result;
use serde_json::Value;
use std::io::Write;

pub(crate) struct Select {
    pub range: Range,
    pub newline: bool,
    /// `None` only for a blank range: the writer copies it verbatim and refuses anything else.
    pub record: Option<Json>,
    /// JSON whitespace only (possibly empty), the one range that is no record and still copied.
    pub blank: bool,
    pub ids: Vec<(u64, u64)>,
    pub replacements: Vec<(Json, Value)>,
    pub root: bool,
}

/// A selected range that is neither one JSON value nor blank. It wraps [`crate::json::InvalidJson`],
/// so `error.is::<InvalidJson>()` still recognises it; the range says where.
#[derive(Debug)]
pub(crate) struct Malformed {
    pub offset: u64,
    pub len: u64,
}
impl Malformed {
    fn at(range: Range) -> Self {
        Self {
            offset: range.offset,
            len: range.len,
        }
    }
}
impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the record at byte {} ({} bytes) is not one JSON value",
            self.offset, self.len
        )
    }
}

impl Select {
    pub fn read(
        source: &JsonSource,
        file: &std::fs::File,
        range: Range,
        newline: bool,
        syntax: bool,
        identity: &Identity,
    ) -> Result<Self> {
        let read = |view: crate::json::JsonView<'_>| {
            Ok((
                view.to_json()?,
                crate::restamp::source_id_spans(view, identity.session_keys)?,
            ))
        };
        // An empty line is blank like any JSON-whitespace range (a JSON range cannot be empty).
        let projected = (range.len != 0).then(|| {
            if syntax {
                source.read(ReadInput::Range(range.offset, range.len), Validation::Syntax, read)
            } else {
                source.read(ReadInput::Range(range.offset, range.len), Validation::Value, read)
            }
        });
        let (record, ids) = match projected.transpose() {
            Ok(Some((record, ids))) => (Some(record), ids),
            Ok(None) => (None, Vec::new()),
            Err(error) if error.is::<crate::json::InvalidJson>() => {
                if ranges::trim(file, range)?.len != 0 {
                    return Err(error.context(Malformed::at(range)));
                }
                (None, Vec::new())
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            range,
            newline,
            blank: record.is_none(),
            record,
            ids,
            replacements: Vec::new(),
            root: false,
        })
    }
}

pub(crate) struct Rewrite<'a> {
    pub session_id: &'a str,
    pub standalone: bool,
    pub identity: Identity,
}

/// The identity keys `harness`'s adapter declares, or an error naming a harness with no
/// derived copies.
pub(crate) fn identity(harness: Harness) -> Result<Identity> {
    crate::harness::rootless(harness)
        .and_then(|adapter| adapter.identity())
        .ok_or_else(|| anyhow::anyhow!("{harness} sessions have no derived copies"))
}
/// Refuse a staged derived copy of `source` that breaks resume where the source did not, before it
/// is published as `destination`: Claude's rules ([`crate::harness::claude::verify_derived`]), the
/// one harness with derived copies. `source_agent` and `agent` are the sub-agent ids each one
/// carries (`None`: a main-thread session). The caller drops the staged file on error, so nothing
/// is left.
pub(crate) fn verify(
    harness: Harness,
    source: &JsonSource,
    file: &std::fs::File,
    source_agent: Option<&str>,
    staged: &std::path::Path,
    destination: &std::path::Path,
    agent: Option<&str>,
) -> Result<()> {
    // One verifier per harness that declares derived copies (a table, like the emit registry).
    match harness {
        Harness::Claude => {
            crate::harness::claude::verify_derived(source, file, source_agent, staged, destination, agent)
        }
        Harness::Codex
        | Harness::Grok
        | Harness::OpenCode
        | Harness::OpenClaw
        | Harness::Gemini
        | Harness::Antigravity
        | Harness::Hermes
        | Harness::LmStudio
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Qwen
        | Harness::Cursor
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => Ok(()),
    }
}

/// The agent id a sub-agent transcript `agent-<id>.jsonl` carries on every record.
pub(crate) fn agent_of(path: &std::path::Path) -> Option<&str> {
    path.file_stem()?.to_str()?.strip_prefix("agent-")
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub lines: usize,
    pub stamped: usize,
    pub bytes: usize,
}
struct Counted<'a, W> {
    output: &'a mut W,
    bytes: usize,
}
impl<W: Write> Write for Counted<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = self.output.write(bytes)?;
        self.bytes = self
            .bytes
            .checked_add(count)
            .ok_or_else(|| std::io::Error::other("derived size overflow"))?;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

/// Derive selected records. An empty selection is a no-op; selectors may stream one record at
/// a time when they fold emitted-byte evidence or decide the next range from prior results.
pub(crate) fn derive(
    source: &JsonSource,
    file: &std::fs::File,
    selected: impl IntoIterator<Item = Result<Select>>,
    rewrite: &Rewrite<'_>,
    output: &mut impl Write,
) -> Result<Counts> {
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    let mut output = Counted { output, bytes: 0 };
    let mut counts = Counts::default();
    for selection in selected {
        let mut selection = selection?;
        let trimmed = ranges::trim(file, selection.range)?;
        counts.lines += usize::from(trimmed.len != 0);
        // Only a blank range may be copied without a checked record; an unparsed line would be
        // one the new session cannot read back. Selectors refuse first; this is the writer's own
        // guarantee, read from the bytes rather than the flag.
        if selection.record.is_none() && (!selection.blank || trimmed.len != 0) {
            return Err(anyhow::Error::new(crate::json::InvalidJson).context(Malformed::at(selection.range)));
        }
        let payload_rewrite = !selection.replacements.is_empty();
        let mut normalize = false;
        if let Some(record) = selection
            .record
            .as_ref()
            .filter(|_| rewrite.standalone || selection.root)
        {
            let keys = &rewrite.identity;
            record.with_view(|view| {
                normalize = rewrite.standalone
                    && (view.get(keys.agent_key)?.is_some()
                        || view
                            .get(keys.sidechain_key)?
                            .map(|v| v.as_bool())
                            .transpose()?
                            .flatten()
                            == Some(true));
                if selection.root {
                    if let Some(parent) = view.get(keys.parent_key)? {
                        if !parent.is_null()? {
                            selection.replacements.push((parent.to_json()?, Value::Null));
                        }
                    }
                }
                Ok(())
            })?;
        }
        if normalize || !selection.replacements.is_empty() {
            let record = selection
                .record
                .as_ref()
                .expect("replacements require a checked record");
            let stamped = record.with_view(|view| {
                if normalize || payload_rewrite {
                    write_modified(view, &selection.replacements, rewrite, &mut output)
                } else {
                    write_pure_reroot(
                        view,
                        source,
                        &selection.ids,
                        rewrite.session_id,
                        &mut selection.replacements,
                        &mut output,
                    )
                }
            })?;
            counts.stamped += usize::from(stamped);
        } else {
            counts.stamped += usize::from(crate::restamp::write_restamped_spans(
                source,
                file,
                selection.range.offset,
                selection.range.len,
                rewrite.session_id,
                &mut selection.ids,
                &mut output,
            )?);
        }
        if selection.newline {
            output.write_all(b"\n")?;
        }
    }
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    counts.bytes = output.bytes;
    Ok(counts)
}

/// A reroot with no other rewrite: restamp every id span the selector validated and otherwise
/// emit the record's own fields untouched. [`write_modified`] would restamp only the session
/// keys and reshape standalone fields, which a pure reroot must not do behind the caller's back.
fn write_pure_reroot(
    view: crate::json::JsonView<'_>,
    source: &JsonSource,
    ids: &[(u64, u64)],
    session_id: &str,
    replacements: &mut Vec<(Json, Value)>,
    writer: &mut impl Write,
) -> Result<bool> {
    for &(start, end) in ids {
        replacements.push((
            source.validated_range(start, end - start)?,
            Value::String(session_id.into()),
        ));
    }
    view.write_canonical_replacing(replacements, writer)?;
    Ok(!ids.is_empty())
}

pub(crate) fn write_modified(
    view: crate::json::JsonView<'_>,
    replacements: &[(Json, Value)],
    rewrite: &Rewrite<'_>,
    writer: &mut impl Write,
) -> Result<bool> {
    let (keys, standalone) = (&rewrite.identity, rewrite.standalone);
    if !view.is_object()? {
        view.write_canonical_replacing(replacements, writer)?;
        return Ok(false);
    }
    writer.write_all(b"{")?;
    let mut first = true;
    let mut stamped = false;
    view.for_each_entry(|key, child| {
        if standalone && key == keys.agent_key {
            return Ok(());
        }
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        serde_json::to_writer(&mut *writer, key)?;
        writer.write_all(b":")?;
        if standalone && key == keys.sidechain_key && child.as_bool()? == Some(true) {
            writer.write_all(b"false")?;
        } else if keys.session_keys.contains(&key) && child.is_string()? {
            serde_json::to_writer(&mut *writer, rewrite.session_id)?;
            stamped = true;
        } else {
            child.write_canonical_replacing(replacements, writer)?;
        }
        Ok(())
    })?;
    writer.write_all(b"}")?;
    Ok(stamped)
}

/// Adoption selects the complete transcript; metadata publication remains the caller's job.
pub(crate) fn transcript(
    source: &JsonSource,
    file: &std::fs::File,
    new_id: &str,
    identity: Identity,
    output: &mut impl Write,
) -> Result<Counts> {
    let selected = ranges::records(
        file,
        Range {
            offset: 0,
            len: source.len(),
        },
    )?
    .map(|row| {
        let (range, newline) = row?;
        // Invalid UTF-8 is not one JSON value: the syntax validator rejects the row as malformed.
        Select::read(source, file, range, newline, true, &identity)
    });
    derive(
        source,
        file,
        selected,
        &Rewrite {
            session_id: new_id,
            standalone: false,
            identity,
        },
        output,
    )
}
