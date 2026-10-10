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
//! The restamp is byte surgery, not a re-serialization: only the top-level `sessionId` /
//! `session_id` string values are replaced, located by offset, so every other byte of every line
//! is the source's. A `sessionId` nested inside content (a tool output quoting a transcript) is
//! content and is not touched.
//!
//! What stays behind: persisted tool outputs (`<dead>/tool-results/*.txt`, named in the transcript
//! by absolute path) and anything `cv prune` stashed in the dead session's sidecar. Both remain
//! readable as long as the dead session's directory is kept.
//!
//! The functions here take the projects root explicitly so they run against fixtures; the CLI
//! passes Claude Code's real store.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::ir::{Harness, SessionRef};
use crate::lanes::{lanes_of, Lane};

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

    fn as_ref(&self) -> SessionRef {
        SessionRef {
            id: self.session_id.clone(),
            harness: Harness::Claude,
            path: self.path.clone(),
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            message_count: 0,
        }
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

fn is_session_transcript(p: &Path) -> Option<String> {
    let name = p.file_name()?.to_str()?;
    if name.ends_with(".flat.jsonl") {
        return None; // a `cv prune` sidecar, not a session
    }
    name.strip_suffix(".jsonl").map(String::from)
}

fn session_files_in(project_dir: &Path) -> Vec<SessionFile> {
    let Ok(rd) = std::fs::read_dir(project_dir) else {
        return vec![];
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter_map(|p| {
            let id = is_session_transcript(&p)?;
            Some(SessionFile {
                session_id: id,
                project_dir: project_dir.to_path_buf(),
                modified: std::fs::metadata(&p).and_then(|m| m.modified()).ok(),
                path: p,
            })
        })
        .collect()
}

fn project_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return vec![];
    };
    rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()
}

/// The project dir Claude Code uses for a working directory (symlinks resolved, as Claude does).
pub fn project_dir_for_cwd(root: &Path, cwd: &Path) -> PathBuf {
    let real = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    root.join(crate::emit::claude_encode_cwd(&real))
}

/// A session by full id or unique prefix, anywhere under `root`.
pub fn find_session(root: &Path, spec: &str) -> Result<SessionFile> {
    if spec.starts_with("agent-") {
        bail!("{spec} is a sub-agent id; this wants a session id");
    }
    let mut hits: Vec<SessionFile> = project_dirs(root)
        .iter()
        .flat_map(|d| session_files_in(d))
        .filter(|s| s.session_id.starts_with(spec))
        .collect();
    if let Some(i) = hits.iter().position(|s| s.session_id == spec) {
        return Ok(hits.swap_remove(i));
    }
    match hits.len() {
        0 => bail!("no Claude session matching {spec:?} under {}", root.display()),
        1 => Ok(hits.pop().unwrap()),
        n => {
            let mut ids: Vec<String> = hits.iter().map(|s| s.session_id.clone()).collect();
            ids.sort();
            bail!("ambiguous session id {spec:?} — {n} candidates:\n{}", ids.join("\n"))
        }
    }
}

/// The most recently written session in a project, skipping `exclude`.
pub fn newest_session(project_dir: &Path, exclude: &[&str]) -> Option<SessionFile> {
    session_files_in(project_dir)
        .into_iter()
        .filter(|s| !exclude.contains(&s.session_id.as_str()))
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

/// Every sub-agent directly under a session's `subagents/` dir (workflow agents, which live one
/// level deeper, are not adoptable this way), oldest first.
pub fn session_agents(session: &SessionFile) -> Vec<AgentFile> {
    let Ok(rd) = std::fs::read_dir(session.subagents_dir()) else {
        return vec![];
    };
    let mut out: Vec<AgentFile> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter_map(|p| agent_file(session, p))
        .collect();
    out.sort_by(|a, b| a.modified.cmp(&b.modified).then(a.agent_id.cmp(&b.agent_id)));
    out
}

/// Every copy of an agent (full id or unique prefix; `agent-` optional) in any session's
/// `subagents/` under `root`, or only in `from` when given. Newest copy first. Copies of ONE agent
/// in several sessions (an earlier adoption) are expected; a prefix matching two different agent
/// ids is an error.
pub fn find_agent(root: &Path, spec: &str, from: Option<&SessionFile>) -> Result<Vec<AgentFile>> {
    let want = spec.strip_prefix("agent-").unwrap_or(spec);
    let sessions: Vec<SessionFile> = match from {
        Some(s) => vec![s.clone()],
        None => project_dirs(root).iter().flat_map(|d| session_files_in(d)).collect(),
    };
    let mut hits: Vec<AgentFile> = Vec::new();
    for s in &sessions {
        let Ok(rd) = std::fs::read_dir(s.subagents_dir()) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let matches = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("agent-{want}")) && n.ends_with(".jsonl"));
            if matches {
                if let Some(a) = agent_file(s, p) {
                    hits.push(a);
                }
            }
        }
    }
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

#[derive(Deserialize)]
struct TopIds<'a> {
    #[serde(borrow, rename = "sessionId")]
    session_id: Option<&'a RawValue>,
    #[serde(borrow, rename = "session_id")]
    session_id_snake: Option<&'a RawValue>,
}

/// Restamp one transcript line: replace the top-level `sessionId` (and `session_id`, the other
/// spelling Claude Code writes) string value with `new_id`, leaving every other byte as it was.
/// Returns `None` when the line carries no top-level session id (it is then kept verbatim).
pub fn restamp_line(line: &str, new_id: &str) -> Option<String> {
    let ids: TopIds = serde_json::from_str(line).ok()?;
    let mut spans: Vec<(usize, usize)> = [ids.session_id, ids.session_id_snake]
        .into_iter()
        .flatten()
        .map(RawValue::get)
        .filter(|raw| raw.starts_with('"'))
        .map(|raw| {
            let start = raw.as_ptr() as usize - line.as_ptr() as usize;
            (start, start + raw.len())
        })
        .collect();
    if spans.is_empty() {
        return None;
    }
    spans.sort();
    let quoted = serde_json::to_string(new_id).expect("a string serializes");
    let mut out = String::with_capacity(line.len() + 8);
    let mut at = 0;
    for (s, e) in spans {
        out.push_str(&line[at..s]);
        out.push_str(&quoted);
        at = e;
    }
    out.push_str(&line[at..]);
    Some(out)
}

/// A restamped transcript and what the restamp touched.
pub struct Restamped {
    pub body: String,
    pub lines: usize,
    pub stamped: usize,
}

/// Restamp a whole transcript. Line endings and blank lines are preserved as they were.
pub fn restamp(text: &str, new_id: &str) -> Restamped {
    let mut body = String::with_capacity(text.len() + 64);
    let (mut lines, mut stamped) = (0, 0);
    for piece in text.split_inclusive('\n') {
        let (line, end) = match piece.strip_suffix('\n') {
            Some(l) => (l, "\n"),
            None => (piece, ""),
        };
        lines += usize::from(!line.trim().is_empty());
        match restamp_line(line, new_id) {
            Some(new) => {
                stamped += 1;
                body.push_str(&new);
            }
            None => body.push_str(line),
        }
        body.push_str(end);
    }
    Restamped { body, lines, stamped }
}

/// One agent's adoption, planned.
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
    let exists = dest.exists() || dest_meta.as_ref().is_some_and(|m| m.exists());
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

/// Restamp the transcript in memory; with `write`, place it and the meta into the target's
/// `subagents/` (each via a temp file and a rename, so a live session never reads half a file).
/// Refuses an existing destination unless `force`.
pub fn execute(p: &Plan, write: bool, force: bool) -> Result<Adopted> {
    if p.agent.session_id == p.into_session {
        bail!(
            "agent {} already belongs to session {}",
            p.agent.agent_id,
            p.into_session
        );
    }
    if p.exists && !force {
        bail!(
            "{} already exists — refusing to overwrite (pass --force to replace it)",
            p.dest.display()
        );
    }
    let text = std::fs::read_to_string(&p.agent.path).with_context(|| format!("reading {}", p.agent.path.display()))?;
    let r = restamp(&text, &p.into_session);
    if write {
        let dir = p.dest.parent().context("destination has no parent dir")?;
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let place = |dest: &Path, bytes: &[u8]| -> Result<()> {
            let tmp = dest.with_extension(format!("adopt-{}.tmp", std::process::id()));
            std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
            std::fs::rename(&tmp, dest).with_context(|| format!("placing {}", dest.display()))
        };
        if let (Some(src), Some(dest)) = (&p.agent.meta_path, &p.dest_meta) {
            place(
                dest,
                &std::fs::read(src).with_context(|| format!("reading {}", src.display()))?,
            )?;
        }
        place(&p.dest, r.body.as_bytes())?;
    }
    Ok(Adopted {
        agent_id: p.agent.agent_id.clone(),
        lines: r.lines,
        stamped: r.stamped,
        bytes: r.body.len(),
    })
}

/// Lanes of `session` keyed for display: status, last text, stranded — from [`lanes_of`].
pub fn lanes_of_session(session: &SessionFile) -> Vec<Lane> {
    lanes_of(&session.as_ref())
}

/// A sub-agent left unfinished by a session that is no longer the live one.
#[derive(Debug, Clone, Serialize)]
pub struct Orphan {
    pub session_id: String,
    pub lane: Lane,
}

/// Unfinished sub-agents of the `recent` most recently written sessions of a project, excluding
/// `live`. Unfinished is [`Lane::is_done`]'s complement on the top-level agents: the harness
/// recorded no completion (`running` — the transcript ends mid-work), recorded it stopped, killed
/// or failed it (a session that exits sends one `stopped` notice naming every lane it abandons),
/// or it completed while parked on a promise (`stranded`).
pub fn orphans(project_dir: &Path, live: &str, recent: usize) -> Vec<Orphan> {
    let mut sessions: Vec<SessionFile> = session_files_in(project_dir)
        .into_iter()
        .filter(|s| s.session_id != live && s.subagents_dir().is_dir())
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.modified));
    sessions.truncate(recent);
    let mut out = Vec::new();
    for s in &sessions {
        let top: std::collections::HashSet<String> = session_agents(s).into_iter().map(|a| a.agent_id).collect();
        for lane in lanes_of_session(s) {
            if top.contains(&lane.agent_id) && !lane.is_done() {
                out.push(Orphan {
                    session_id: s.session_id.clone(),
                    lane,
                });
            }
        }
    }
    out
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

    #[test]
    fn adopt_restamps_only_the_session_id_and_copies_the_meta() {
        let root = fixture("rewrite");
        let into = find_session(&root, "2222").unwrap();
        assert_eq!(into.session_id, LIVE);
        let hits = find_agent(&root, &format!("agent-{}", &AGENT[..6]), None).unwrap();
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
    fn adopt_refuses_to_overwrite_without_force() {
        let root = fixture("refuse");
        let into = find_session(&root, LIVE).unwrap();
        let agent = find_agent(&root, AGENT, None).unwrap().remove(0);
        execute(&plan(agent.clone(), vec![], &into), true, false).unwrap();

        // The agent now lives in both sessions; the search sees both copies, newest first.
        let copies = find_agent(&root, AGENT, None).unwrap();
        assert_eq!(copies.len(), 2);

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
        let dead = find_session(&root, DEAD).unwrap();
        assert!(execute(&plan(agent, vec![], &dead), true, true).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn newest_session_and_listing() {
        let root = fixture("list");
        let proj = root.join("-tmp-proj");
        let dead = find_session(&root, DEAD).unwrap();
        let agents = session_agents(&dead);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].description.as_deref(), Some("fixture lane"));
        assert!(agents[0].meta_path.is_some());
        let newest = newest_session(&proj, &[]).unwrap().session_id;
        assert!(newest == DEAD || newest == LIVE);
        assert_ne!(newest_session(&proj, &[newest.as_str()]).unwrap().session_id, newest);
        assert!(find_session(&root, &format!("agent-{AGENT}")).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn restamp_line_ignores_lines_without_a_top_level_id() {
        assert_eq!(restamp_line("not json", LIVE), None);
        assert_eq!(restamp_line(r#"{"a":{"sessionId":"x"}}"#, LIVE), None);
        assert_eq!(restamp_line(r#"{"sessionId":null}"#, LIVE), None);
        assert_eq!(
            restamp_line(r#"{"sessionId":"x","session_id":"x","k":"x"}"#, "y").unwrap(),
            r#"{"sessionId":"y","session_id":"y","k":"x"}"#
        );
    }
}
