//! `cv adopt` — move a sub-agent stranded by a dead Claude Code session into a live one, so the
//! live session can `SendMessage` it and it resumes with its whole transcript.
//!
//! Claude Code keeps a sub-agent at `projects/<slug>/<session>/subagents/agent-<id>.jsonl`, with
//! `agent-<id>.meta.json` (agent type, description, model, …) beside it, and stamps the owning
//! session's id on every line as `sessionId`. A session resolves `SendMessage` to an agent id from
//! its own `subagents/` dir, so adoption is: copy the transcript into the live session's dir with
//! the session id restamped, and copy the meta verbatim. Nothing else changes — the `cwd`,
//! `agentId`, uuids and every tool output stay as they were.
//!
//! The restamp is [`crate::restamp`]'s byte surgery, not a re-serialization: only the top-level
//! `sessionId` / `session_id` string values are replaced, so every other byte of every line is the
//! source's.
//!
//! What stays behind: persisted tool outputs (`<dead>/tool-results/*.txt`, named in the transcript
//! by absolute path) and anything `cv prune` stashed in the dead session's sidecar. Both remain
//! readable as long as the dead session's directory is kept.
//!
//! The fleet is read the way every other command reads it: sessions through [`crate::sessions`] /
//! [`crate::find`] (the catalog, kept fresh by its probe), agents through
//! [`crate::find_subagent_parents`] and each parent's `subagents/` dir. [`plan`] and [`execute`]
//! take resolved files, so they run against fixtures.

use crate::ir::{Harness, SessionRef};
#[cfg(test)]
use crate::restamp::restamp;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One session transcript in the store.
#[derive(Debug, Clone, Serialize)]
pub struct SessionFile {
    pub session_id: String,
    /// `projects/<slug>` — the directory holding `<session_id>.jsonl` and `<session_id>/`.
    pub project_dir: PathBuf,
    pub path: PathBuf,
    #[serde(skip)]
    pub modified: Option<SystemTime>,
}

impl SessionFile {
    pub fn subagents_dir(&self) -> PathBuf {
        self.project_dir.join(&self.session_id).join("subagents")
    }
}

/// One sub-agent transcript directly under a session's `subagents/` dir.
#[derive(Debug, Clone, Serialize)]
pub struct AgentFile {
    /// The bare agent id (no `agent-` prefix) — what `SendMessage` takes.
    pub agent_id: String,
    pub session_id: String,
    pub project_dir: PathBuf,
    pub path: PathBuf,
    /// The `.meta.json` sidecar, when the session wrote one.
    pub meta_path: Option<PathBuf>,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip)]
    pub modified: Option<SystemTime>,
}

impl SessionFile {
    /// The file behind a Claude session ref, with its mtime (what "newest" means here: a session
    /// that just crashed was written last).
    fn of(r: SessionRef) -> Option<SessionFile> {
        let project_dir = r.path.parent()?.to_path_buf();
        Some(SessionFile {
            modified: std::fs::metadata(&r.path).and_then(|m| m.modified()).ok(),
            session_id: r.id,
            project_dir,
            path: r.path,
        })
    }
}

/// A Claude session by full id or unique prefix, as [`crate::find`] resolves it.
pub fn find_session(spec: &str) -> Result<SessionFile> {
    if spec.starts_with("agent-") {
        bail!("{spec} is a sub-agent id; this wants a session id");
    }
    match crate::find(spec, Some(Harness::Claude))? {
        // Check the resolved id too: a short selector need not itself start with agent-.
        Some((r, _)) if !r.id.starts_with("agent-") => {
            SessionFile::of(r).with_context(|| format!("{spec}: the transcript has no project dir"))
        }
        _ => bail!("no Claude session matching {spec:?}"),
    }
}

/// The most recently written Claude session for one project slug (`-work-proj`) across every
/// root, skipping `exclude`: the same working directory is one project in each Claude config dir.
pub fn newest_session(project_slug: &std::ffi::OsStr, exclude: &[&str]) -> Option<SessionFile> {
    crate::sessions()
        .into_iter()
        .filter(|r| r.harness == Harness::Claude && !exclude.contains(&r.id.as_str()))
        .filter(|r| r.path.parent().and_then(Path::file_name) == Some(project_slug))
        .filter_map(SessionFile::of)
        .max_by_key(|s| s.modified)
}

fn read_meta(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn agent_file(session: &SessionFile, path: PathBuf) -> Option<AgentFile> {
    let name = path.file_name()?.to_str()?;
    if name.ends_with(".flat.jsonl") {
        return None;
    }
    let agent_id = name.strip_prefix("agent-")?.strip_suffix(".jsonl")?.to_string();
    let meta_path = path.with_file_name(format!("agent-{agent_id}.meta.json"));
    let meta = read_meta(&meta_path);
    let field = |k: &str| {
        meta.as_ref()
            .and_then(|m| m.get(k))
            .and_then(Value::as_str)
            .map(String::from)
    };
    let md = std::fs::metadata(&path).ok();
    Some(AgentFile {
        session_id: session.session_id.clone(),
        project_dir: session.project_dir.clone(),
        bytes: md.as_ref().map(|m| m.len()).unwrap_or(0),
        modified: md.and_then(|m| m.modified().ok()),
        description: field("description"),
        agent_type: field("agentType"),
        model: field("model"),
        meta_path: meta_path.is_file().then_some(meta_path),
        agent_id,
        path,
    })
}

/// Every sub-agent directly under a session's `subagents/` dir whose file name starts with
/// `agent-<want>` (workflow agents, which live one level deeper, are not adoptable this way).
fn agents_in(session: &SessionFile, want: &str) -> Vec<AgentFile> {
    let Ok(rd) = std::fs::read_dir(session.subagents_dir()) else {
        return vec![];
    };
    let prefix = format!("agent-{want}");
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".jsonl") && !n.ends_with(".flat.jsonl"))
        })
        .filter_map(|p| agent_file(session, p))
        .collect()
}

/// Every copy of an agent (full id or unique prefix; `agent-` optional) in any session's
/// `subagents/` ([`crate::find_subagent_parents`]), or only in `from` when given. Newest copy
/// first. Copies of ONE agent in several sessions (an earlier adoption) are expected; a prefix
/// matching two different agent ids is an error unless an exact id matches, which wins first.
pub fn find_agent(spec: &str, from: Option<&SessionFile>) -> Result<Vec<AgentFile>> {
    let want = spec.strip_prefix("agent-").unwrap_or(spec);
    let sessions: Vec<SessionFile> = match from {
        Some(s) => vec![s.clone()],
        None => crate::find_subagent_parents(want)
            .into_iter()
            .filter_map(SessionFile::of)
            .collect(),
    };
    let mut hits: Vec<AgentFile> = sessions.iter().flat_map(|s| agents_in(s, want)).collect();
    if hits.iter().any(|a| a.agent_id == want) {
        hits.retain(|a| a.agent_id == want);
    }
    let mut ids: Vec<&str> = hits.iter().map(|a| a.agent_id.as_str()).collect();
    ids.sort();
    ids.dedup();
    if ids.len() > 1 {
        bail!(
            "agent prefix {spec:?} matches {} agents: {} — pass a longer id",
            ids.len(),
            ids.join(", ")
        );
    }
    hits.sort_by_key(|a| std::cmp::Reverse(a.modified));
    Ok(hits)
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub agent: AgentFile,
    pub into_session: String,
    pub dest: PathBuf,
    /// Where the meta goes (`None` when the source has no meta to copy).
    pub dest_meta: Option<PathBuf>,
    /// The destination already holds this agent (transcript or meta).
    pub exists: bool,
    /// Other sessions that also hold a copy of this agent (not chosen: older).
    pub other_copies: Vec<String>,
}

pub fn plan(agent: AgentFile, other_copies: Vec<String>, into: &SessionFile) -> Plan {
    let dir = into.subagents_dir();
    let dest = dir.join(format!("agent-{}.jsonl", agent.agent_id));
    let dest_meta = agent
        .meta_path
        .as_ref()
        .map(|_| dir.join(format!("agent-{}.meta.json", agent.agent_id)));
    // Metadata reserves the destination even when the chosen source has no sidecar.
    let exists =
        std::fs::symlink_metadata(&dest).is_ok() || std::fs::symlink_metadata(dest.with_extension("meta.json")).is_ok();
    Plan {
        agent,
        into_session: into.session_id.clone(),
        dest,
        dest_meta,
        exists,
        other_copies,
    }
}

/// What an adoption wrote (or, dry, would write).
#[derive(Debug, Clone, Serialize)]
pub struct Adopted {
    pub agent_id: String,
    pub lines: usize,
    pub stamped: usize,
    pub bytes: usize,
}

/// Stream checked transcript surgery into private staging; with `write`, publish the transcript
/// and optional metadata through exclusive temporary files. Source failures occur before publication.
/// Refuses existing destinations unless `force`, including names claimed after planning.
pub fn execute(p: &Plan, write: bool, force: bool) -> Result<Adopted> {
    execute_after_stage(p, write, force, || Ok(()))
}

fn execute_after_stage(
    p: &Plan,
    write: bool,
    force: bool,
    after_stage: impl FnOnce() -> Result<()>,
) -> Result<Adopted> {
    if p.agent.session_id == p.into_session {
        bail!(
            "agent {} already belongs to session {}",
            p.agent.agent_id,
            p.into_session
        );
    }
    use crate::publish::refuse_existing;
    const HINT: &str = " (pass --force to replace it)";
    let refuse_destinations = || -> Result<()> {
        refuse_existing(&p.dest, HINT)?;
        refuse_existing(&p.dest.with_extension("meta.json"), HINT)
    };
    if !force {
        if p.exists {
            return Err(crate::publish::Exists {
                path: p.dest.clone(),
                hint: HINT,
            }
            .into());
        }
        refuse_destinations()?;
    }
    use crate::publish::{Claim, Staged};
    let source = crate::json::JsonSource::capture(&p.agent.path)
        .with_context(|| format!("reading {}", p.agent.path.display()))?;
    let file = std::fs::File::open(&p.agent.path)?;
    source.check_metadata(file.metadata()?)?;
    let mut transcript = None;
    let mut metadata = None;
    let counts = if write {
        let dir = p.dest.parent().context("destination has no parent dir")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut output = std::io::BufWriter::new(Staged::create(dir, "adopt", 0o600)?);
        let counts = crate::derive::transcript(&source, &file, &p.into_session, super::IDENTITY, &mut output)
            .with_context(|| format!("adopting {}", p.agent.path.display()))?;
        transcript = Some(output.into_inner().map_err(|e| e.into_error())?);
        if let (Some(src), Some(dest)) = (&p.agent.meta_path, &p.dest_meta) {
            let meta_source =
                crate::json::JsonSource::capture(src).with_context(|| format!("reading {}", src.display()))?;
            let mut meta_file = std::fs::File::open(src)?;
            meta_source.check_metadata(meta_file.metadata()?)?;
            let mut output = Staged::create(dir, "adopt", 0o600)?;
            std::io::copy(&mut meta_file, &mut output)?;
            metadata = Some((meta_source, meta_file, output, dest));
        }
        counts
    } else {
        crate::derive::transcript(&source, &file, &p.into_session, super::IDENTITY, &mut std::io::sink())
            .with_context(|| format!("adopting {}", p.agent.path.display()))?
    };
    after_stage()?;
    source.check_metadata(file.metadata()?)?;
    source.check()?;
    if let Some((meta_source, meta_file, _, _)) = &metadata {
        meta_source.check_metadata(meta_file.metadata()?)?;
        meta_source.check()?;
    }
    if let Some(staged) = &transcript {
        crate::derive::verify(
            Harness::Claude,
            &source,
            &file,
            Some(&p.agent.agent_id),
            staged.path(),
            &p.dest,
            Some(&p.agent.agent_id),
        )?;
    }
    if write && !force {
        refuse_destinations()?;
    }
    let claim = if force { Claim::Replace } else { Claim::New };
    // One set, the metadata first: a published transcript always has its metadata.
    let mut set = crate::publish::Set::new();
    if let Some((_, _, staged, destination)) = metadata {
        set.add(staged, destination, claim);
    }
    if let Some(staged) = transcript {
        set.add(staged, &p.dest, claim);
    }
    set.publish()?;
    Ok(Adopted {
        agent_id: p.agent.agent_id.clone(),
        lines: counts.lines,
        stamped: counts.stamped,
        bytes: counts.bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEAD: &str = "11111111-1111-4111-8111-111111111111";
    const LIVE: &str = "22222222-2222-4222-8222-222222222222";
    const AGENT: &str = "a0123456789abcdef";

    /// A fixture store: one project with a dead session owning a sub-agent and a live session.
    /// The agent's lines carry the dead id at the top level, once nested inside a tool output
    /// (which must survive verbatim), with a non-ASCII payload and spacing serde would normalize.
    fn fixture(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cv-adopt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let proj = root.join("-tmp-proj");
        let sub = proj.join(DEAD).join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            proj.join(format!("{DEAD}.jsonl")),
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"{DEAD}\",\"message\":{{\"role\":\"user\",\"content\":\"go\"}}}}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            proj.join(format!("{LIVE}.jsonl")),
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"{LIVE}\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n"
            ),
        )
        .unwrap();
        let agent = [
            format!("{{\"parentUuid\":null,\"isSidechain\":true,\"agentId\":\"{AGENT}\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"brief — ☃\"}},\"uuid\":\"u1\",\"sessionId\":\"{DEAD}\"}}"),
            format!("{{\"parentUuid\":\"u1\",\"isSidechain\":true,\"agentId\":\"{AGENT}\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"content\":\"{{\\\"sessionId\\\":\\\"{DEAD}\\\"}}\"}}]}},\"uuid\":\"u2\",\"sessionId\": \"{DEAD}\" , \"n\":1.50}}"),
            "{\"type\":\"summary\",\"summary\":\"no session id here\"}".to_string(),
        ]
        .join("\n")
            + "\n";
        std::fs::write(sub.join(format!("agent-{AGENT}.jsonl")), agent).unwrap();
        std::fs::write(
            sub.join(format!("agent-{AGENT}.meta.json")),
            r#"{"agentType":"general-purpose","description":"fixture lane","model":"opus"}"#,
        )
        .unwrap();
        root
    }

    /// The fixture's session `id` as the fleet lookup would resolve it.
    fn sf(root: &Path, id: &str) -> SessionFile {
        SessionFile::of(SessionRef::from_path(
            root.join("-tmp-proj").join(format!("{id}.jsonl")),
            Harness::Claude,
        ))
        .unwrap()
    }

    #[test]
    fn adopt_restamps_only_the_session_id_and_copies_the_meta() {
        let root = fixture("rewrite");
        let into = sf(&root, LIVE);
        let hits = find_agent(&format!("agent-{}", &AGENT[..6]), Some(&sf(&root, DEAD))).unwrap();
        assert_eq!(hits.len(), 1);
        let src_text = std::fs::read_to_string(&hits[0].path).unwrap();
        let p = plan(hits[0].clone(), vec![], &into);
        assert!(!p.exists);

        // Dry: the plan is computed, nothing is written.
        let dry = execute(&p, false, false).unwrap();
        assert_eq!((dry.lines, dry.stamped), (3, 2));
        assert!(!p.dest.exists());

        execute(&p, true, false).unwrap();
        let out = std::fs::read_to_string(&p.dest).unwrap();
        // Exactly the top-level ids changed: swapping them back gives the source bytes.
        assert_eq!(out.matches(LIVE).count(), 2);
        let undo = restamp(&out, DEAD).body;
        assert_eq!(undo, src_text);
        // The id quoted inside a tool output is content and stays the dead one; spacing, the
        // `1.50` literal and the non-ASCII text survive byte for byte.
        assert!(out.contains(&format!("{{\\\"sessionId\\\":\\\"{DEAD}\\\"}}")));
        assert!(out.contains(&format!("\"sessionId\": \"{LIVE}\" , \"n\":1.50}}")));
        assert!(out.contains("brief — ☃"));
        assert!(out.ends_with("\"no session id here\"}\n"));
        assert_eq!(
            std::fs::read(p.dest_meta.as_ref().unwrap()).unwrap(),
            std::fs::read(hits[0].meta_path.as_ref().unwrap()).unwrap()
        );
        // The source is untouched.
        assert_eq!(std::fs::read_to_string(&hits[0].path).unwrap(), src_text);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn adopt_streams_large_and_blank_rows_with_exact_old_output_and_dry_counts() {
        let root = fixture("streamed-output");
        let into = sf(&root, LIVE);
        let dead = sf(&root, DEAD);
        let agent = find_agent(AGENT, Some(&dead)).unwrap().remove(0);
        let original = format!(
            "\r\n{{ \"sessionId\" : \"{DEAD}\", \"text\":\"{}\", \"n\":1.50 }}\r\n{}\n{{\"session_id\":\"{DEAD}\"}}",
            "λ\\n".repeat(40000),
            " ".repeat(100000)
        );
        std::fs::write(&agent.path, &original).unwrap();
        let expected = restamp(&original, LIVE);
        let p = plan(agent, vec![], &into);
        let dry = execute(&p, false, false).unwrap();
        assert!(!p.dest.parent().unwrap().exists());
        let actual = execute(&p, true, false).unwrap();
        assert_eq!(std::fs::read(&p.dest).unwrap(), expected.body.as_bytes());
        assert_eq!(std::fs::read(&p.agent.path).unwrap(), original.as_bytes());
        for result in [dry, actual] {
            assert_eq!(
                (result.lines, result.stamped, result.bytes),
                (expected.lines, expected.stamped, expected.body.len())
            );
        }
        cleanup_owned_test_root(&root);
    }

    /// A malformed line between two readable records is a record to no reader; an adoption that
    /// copied it verbatim would publish an agent transcript Claude Code cannot load. Refuse first,
    /// naming the file and the byte, and publish nothing.
    #[test]
    fn adopt_refuses_a_malformed_line_inside_the_transcript() {
        let root = fixture("malformed-inside");
        let into = sf(&root, LIVE);
        let dead = sf(&root, DEAD);
        let agent = find_agent(AGENT, Some(&dead)).unwrap().remove(0);
        let first = format!(
            "{{\"parentUuid\":null,\"isSidechain\":true,\"agentId\":\"{AGENT}\",\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"brief\"}},\"uuid\":\"u1\",\"sessionId\":\"{DEAD}\"}}\n"
        );
        let original = format!(
            "{first}{{\"parentUuid\":\"u1\",\"isSidechain\":true,\"agentId\":\"{AGENT}\",\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":\"cut sho\n\
             {{\"parentUuid\":\"u1\",\"isSidechain\":true,\"agentId\":\"{AGENT}\",\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":\"ok\"}},\"uuid\":\"a1\",\"sessionId\":\"{DEAD}\"}}\n"
        );
        std::fs::write(&agent.path, &original).unwrap();
        let p = plan(agent, vec![], &into);
        for write in [true, false] {
            let error = execute(&p, write, false).unwrap_err();
            assert!(error.is::<crate::json::InvalidJson>(), "{error:#}");
            let message = format!("{error:#}");
            assert!(message.contains(&p.agent.path.display().to_string()), "{message}");
            assert!(message.contains(&format!("byte {}", first.len())), "{message}");
            assert!(!p.dest.exists(), "nothing is published when the source is unreadable");
        }
        assert_eq!(std::fs::read(&p.agent.path).unwrap(), original.as_bytes());
        cleanup_owned_test_root(&root);
    }

    fn cleanup_owned_test_root(root: &Path) {
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn adopt_source_and_metadata_failures_do_not_publish_partial_outputs() {
        for failure in [
            "invalid-utf8",
            "invalid-large-utf8",
            "missing-meta",
            "changed-source",
            "changed-meta",
        ] {
            let root = fixture(failure);
            let into = sf(&root, LIVE);
            let dead = sf(&root, DEAD);
            let agent = find_agent(AGENT, Some(&dead)).unwrap().remove(0);
            let p = plan(agent, vec![], &into);
            let original = std::fs::read(&p.agent.path).unwrap();
            let metadata = p.agent.meta_path.as_ref().unwrap();
            let destination_metadata = p.dest_meta.as_ref().unwrap();
            let prior_transcript = b"previous destination transcript";
            let prior_metadata = b"previous destination metadata";
            std::fs::create_dir_all(p.dest.parent().unwrap()).unwrap();
            std::fs::write(&p.dest, prior_transcript).unwrap();
            std::fs::write(destination_metadata, prior_metadata).unwrap();
            match failure {
                "invalid-utf8" => {
                    let mut bytes = original.clone();
                    bytes.extend_from_slice(b"\nlate invalid \xff");
                    std::fs::write(&p.agent.path, bytes).unwrap();
                }
                "invalid-large-utf8" => {
                    let mut bytes = original.clone();
                    bytes.extend_from_slice(&vec![b'x'; 100000]);
                    bytes.push(0xff);
                    std::fs::write(&p.agent.path, bytes).unwrap();
                }
                "missing-meta" => {
                    std::fs::remove_file(metadata).unwrap();
                }
                _ => {}
            }
            let result = execute_after_stage(&p, true, true, || {
                match failure {
                    "changed-source" => std::fs::write(&p.agent.path, b"source changed")?,
                    "changed-meta" => std::fs::write(metadata, b"metadata changed")?,
                    _ => {}
                }
                Ok(())
            });
            assert!(result.is_err(), "{failure}");
            assert_eq!(std::fs::read(&p.dest).unwrap(), prior_transcript, "{failure}");
            assert_eq!(
                std::fs::read(destination_metadata).unwrap(),
                prior_metadata,
                "{failure}"
            );
            assert!(std::fs::read_dir(p.dest.parent().unwrap())
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(".cv-adopt-")));
            cleanup_owned_test_root(&root);
        }
    }

    /// The staged transcript is verified resumable against the agent's own before it is published:
    /// a break the source does not have refuses the adoption and publishes neither file.
    #[test]
    fn a_staged_adoption_that_breaks_resume_is_refused_and_leaves_nothing() {
        let root = fixture("breaks-resume");
        let into = sf(&root, LIVE);
        let agent = find_agent(AGENT, Some(&sf(&root, DEAD))).unwrap().remove(0);
        let p = plan(agent, vec![], &into);
        let dir = p.dest.parent().unwrap().to_path_buf();
        let err = execute_after_stage(&p, true, false, || crate::prune::tests::break_staged(&dir, "adopt"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("parentUuid nowhere names no record"), "{err}");
        assert!(!p.dest.exists());
        assert!(!p.dest.with_extension("meta.json").exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        execute(&p, true, false).unwrap();
        cleanup_owned_test_root(&root);
    }

    #[test]
    fn adopt_rechecks_late_metadata_even_when_source_has_no_sidecar() {
        let root = fixture("late-meta-no-source");
        let into = sf(&root, LIVE);
        let dead = sf(&root, DEAD);
        let mut agent = find_agent(AGENT, Some(&dead)).unwrap().remove(0);
        agent.meta_path = None;
        let p = plan(agent, vec![], &into);
        let meta = p.dest.with_extension("meta.json");
        let result = execute_after_stage(&p, true, false, || {
            std::fs::write(&meta, b"concurrent metadata")?;
            Ok(())
        });
        assert!(result.unwrap_err().to_string().contains("refusing to overwrite"));
        assert!(!p.dest.exists());
        assert_eq!(std::fs::read(&meta).unwrap(), b"concurrent metadata");
        cleanup_owned_test_root(&root);
    }

    #[test]
    fn adopt_rechecks_destinations_created_after_planning() {
        let root = fixture("late-destination");
        let into = sf(&root, LIVE);
        let dead = sf(&root, DEAD);
        let agent = find_agent(AGENT, Some(&dead)).unwrap().remove(0);
        let p = plan(agent, vec![], &into);
        assert!(!p.exists);
        std::fs::create_dir_all(p.dest.parent().unwrap()).unwrap();
        for destination in [&p.dest, p.dest_meta.as_ref().unwrap()] {
            std::fs::write(destination, "another writer's artifact").unwrap();
            assert!(execute(&p, true, false)
                .unwrap_err()
                .to_string()
                .contains("refusing to overwrite"));
            assert_eq!(std::fs::read(destination).unwrap(), b"another writer's artifact");
            std::fs::remove_file(destination).unwrap();
        }
        assert!(!p.dest.exists());
        assert!(!p.dest_meta.as_ref().unwrap().exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn adopt_source_without_metadata_respects_existing_destination_metadata() {
        let root = fixture("metadata-reservation");
        let into = sf(&root, LIVE);
        let dead = sf(&root, DEAD);
        let mut agent = find_agent(AGENT, Some(&dead)).unwrap().remove(0);
        agent.meta_path = None;
        std::fs::create_dir_all(into.subagents_dir()).unwrap();
        let meta = into.subagents_dir().join(format!("agent-{AGENT}.meta.json"));
        std::fs::write(&meta, "reserved metadata").unwrap();
        let p = plan(agent, vec![], &into);
        assert!(p.exists);
        assert!(p.dest_meta.is_none());
        assert!(execute(&p, true, false).is_err());
        assert!(!p.dest.exists());
        assert_eq!(std::fs::read(&meta).unwrap(), b"reserved metadata");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn adopt_refuses_to_overwrite_without_force() {
        let root = fixture("refuse");
        let into = sf(&root, LIVE);
        let dead = sf(&root, DEAD);
        let agent = find_agent(AGENT, Some(&dead)).unwrap().remove(0);
        execute(&plan(agent.clone(), vec![], &into), true, false).unwrap();

        // The agent now lives in both sessions.
        assert_eq!(agents_in(&dead, AGENT).len() + agents_in(&into, AGENT).len(), 2);

        // A sentinel in the adopted copy shows whether a refused run wrote anything.
        let p = plan(agent.clone(), vec![], &into);
        assert!(p.exists);
        std::fs::write(&p.dest, "sentinel\n").unwrap();
        let err = execute(&p, true, false).unwrap_err().to_string();
        assert!(err.contains("refusing to overwrite"), "{err}");
        assert!(execute(&p, false, false).is_err(), "a dry run refuses too");
        assert_eq!(std::fs::read_to_string(&p.dest).unwrap(), "sentinel\n");

        execute(&p, true, true).unwrap();
        assert!(std::fs::read_to_string(&p.dest).unwrap().contains(LIVE));

        // Adopting into the session that already owns the agent is refused.
        assert!(execute(&plan(agent, vec![], &dead), true, true).is_err());
        std::fs::remove_dir_all(&root).ok();
    }
}
