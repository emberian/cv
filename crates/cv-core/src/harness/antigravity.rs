//! Google Antigravity (`agy` CLI, Antigravity 2.0, Antigravity IDE) adapter.
//!
//! ## Storage topology (`third_party/jetski/cortex/trajectory_store/`)
//! Root is `$JETSKI_APP_DATA_DIR` when set, otherwise the existing directories among:
//! 1. `~/.gemini/antigravity-cli` (Antigravity CLI, `agy`)
//! 2. `~/.gemini/antigravity` (Antigravity 2.0 hub/app)
//! 3. `~/.gemini/antigravity-ide` (Antigravity IDE)
//!
//! Because `antigravity` and `antigravity-ide` can mirror or cross-index `antigravity-cli`
//! trajectories by UUID, discovery deduplicates across roots by `<uuid>`, preferring the richest
//! unencrypted step log per conversation:
//! `brain/<uuid>/.system_generated/logs/transcript_full.jsonl`
//!   > `transcript.jsonl`
//!   > `overview.txt` (legacy 1.x NDJSON using the same step schema).
//!
//! Explicitly skipped sidecars:
//! - `brain/<uuid>/.system_generated/logs/chunks/**` (chunked byte-identical copy of `transcript_full.jsonl`)
//! - `implicit/<uuid>.pb` (AEAD-encrypted implicit context blobs, not sessions)
//! - `conversations/<uuid>.pb` (Keychain AEAD-encrypted `ProtoStore` trajectory blobs; `cv` never
//!   touches OS Keychains and reads `overview.txt` + plaintext SQLite/pbtxt sidecars instead)
//! - `history.jsonl` (prompt input history, not session transcripts)
//!
//! ## Optional metadata sidecars (all read-only)
//! - `annotations/<uuid>.pbtxt` — textproto `title: "..."` (highest-priority session title).
//! - `brain/<uuid>/.system_generated/subagents/<child-uuid>.json` — child subagent descriptors
//!   (`conversationId`, `spawnStepIndex`, `state`, `subagentDescriptor{typeName, role}`,
//!   `workspaceUris`).
//! - `conversation_summaries.db` (`#[cfg(feature = "sqlite")]`, opened via [`super::store::sqlite`])
//!   — `conversation_summaries` catalog (`title`, `preview`, `workspace_uris`,
//!   `parent_conversation_id`, `agent_name`, `source`, `project_id`, `last_modified_time`).
//! - `conversations/<uuid>.db` (`#[cfg(feature = "sqlite")]`, opened via [`super::store::sqlite`])
//!   — `gen_metadata` (per-step model slug in protobuf field `1.19`, step indices in field `2`)
//!   and `trajectory_metadata_blob` (workspace URI `1.1`, git root `1.2`, git branch `1.4`,
//!   created timestamp `2`).

use super::store::document::Fields;
use super::store::jsonl::{self, Codec as _};
use super::Adapter;
use crate::affordance::Denial;
use crate::ir::*;
use crate::json::{Json, JsonView};
use crate::lazy::Text;
use crate::source::{Dependencies, Probe, ReadSet};
use crate::stream::{Demand, Flow, MessageSink, Start, Tier};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
#[cfg(feature = "sqlite")]
use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

const APP_DATA_SUBDIRS: &[&str] = &["antigravity-cli", "antigravity", "antigravity-ide"];

const DENIALS: &[Denial] = &[
    Denial {
        prefix: "permission check failed",
        also: None,
    },
    Denial {
        prefix: "Encountered error in tool execution:",
        also: Some("permission check failed"),
    },
    Denial {
        prefix: "Created At:",
        also: Some("permission check failed"),
    },
    Denial {
        prefix: "User denied",
        also: None,
    },
    Denial {
        prefix: "Encountered error in tool execution:",
        also: Some("User denied"),
    },
    Denial {
        prefix: "Created At:",
        also: Some("User denied"),
    },
];

pub struct Antigravity {
    roots: Vec<PathBuf>,
}

/// Rootless instance for static capability/affordance queries (`super::rootless`).
pub(crate) static ROOTLESS: Antigravity = Antigravity { roots: Vec::new() };

impl Antigravity {
    pub fn detect() -> Self {
        if let Some(explicit) = crate::env::var_os("JETSKI_APP_DATA_DIR") {
            let dir = PathBuf::from(explicit);
            return Antigravity {
                roots: dir.exists().then_some(dir).into_iter().collect(),
            };
        }
        let roots = crate::env::home_dir()
            .map(|h| h.join(".gemini"))
            .map(|gemini| existing_app_dirs(&gemini))
            .unwrap_or_default();
        Antigravity { roots }
    }

    /// Construct an adapter scoped strictly inside `root`:
    /// - `<home>/.gemini/{antigravity-cli,antigravity,antigravity-ide}` when present,
    /// - `<gemini>/{antigravity-cli,antigravity,antigravity-ide}` when present,
    /// - otherwise `root` itself as an `<appDataDir>` (or flat JSONL directory).
    pub fn with_home(root: &Path) -> Self {
        if !root.exists() {
            return Antigravity { roots: Vec::new() };
        }
        let dot_gemini = root.join(".gemini");
        if dot_gemini.is_dir() {
            let sub = existing_app_dirs(&dot_gemini);
            if !sub.is_empty() {
                return Antigravity { roots: sub };
            }
        }
        let direct_sub = existing_app_dirs(root);
        if !direct_sub.is_empty() {
            return Antigravity { roots: direct_sub };
        }
        Antigravity {
            roots: vec![root.to_path_buf()],
        }
    }
}

fn existing_app_dirs(gemini_dir: &Path) -> Vec<PathBuf> {
    APP_DATA_SUBDIRS
        .iter()
        .map(|sub| gemini_dir.join(sub))
        .filter(|p| p.is_dir())
        .collect()
}

impl Adapter for Antigravity {
    fn harness(&self) -> Harness {
        Harness::Antigravity
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    fn capabilities(&self) -> super::Capabilities {
        super::Capabilities {
            denials: DENIALS,
            edit_guard: None,
            lane_stops: None,
            deferred_wrapper: None,
            rich_spans: true,
        }
    }

    fn append_only(&self, r: &SessionRef) -> bool {
        r.path.is_file()
    }

    fn dependencies(&self, r: &SessionRef, _probe: &mut Probe, deps: &mut Dependencies) -> Option<()> {
        if let Some(loc) = BrainLoc::from_transcript(&r.path) {
            deps.files
                .push(loc.app_data_dir.join("annotations").join(format!("{}.pbtxt", loc.id)));
            deps.directories
                .push(loc.brain_dir.join(".system_generated").join("subagents"));
            #[cfg(feature = "sqlite")]
            {
                let conv_db = loc.app_data_dir.join("conversations").join(format!("{}.db", loc.id));
                record_sqlite_deps(&conv_db, &mut deps.files);
            }
        }
        Some(())
    }

    fn delivered_body<'a>(&self, m: &Message, text: &'a str) -> Option<&'a str> {
        if m.origin == Origin::Subagent || m.kind == MessageKind::InjectedContext {
            return unwrap_system_message_body(text);
        }
        None
    }

    fn agent_transcript(&self, parent: &SessionRef, agent_session: &str, _workflow: Option<&str>) -> Option<PathBuf> {
        if agent_session.is_empty() || agent_session.contains(['/', '\\']) {
            return None;
        }
        let loc = BrainLoc::from_transcript(&parent.path)?;
        select_brain_log(&loc.app_data_dir.join("brain").join(agent_session)).map(|(_, path)| path)
    }

    fn forest(
        &self,
        r: &SessionRef,
        _demand: crate::SubagentDemand,
        workflow: Option<&str>,
        select: &(dyn Fn(&str, Option<&str>) -> bool + Sync),
    ) -> Result<Vec<crate::AgentRef>> {
        if workflow.is_some() {
            return Ok(Vec::new());
        }
        let Some(loc) = BrainLoc::from_transcript(&r.path) else {
            return Ok(Vec::new());
        };
        let descriptors = read_subagent_descriptors(&loc.brain_dir)?;
        if descriptors.is_empty() {
            return Ok(Vec::new());
        }
        #[cfg(feature = "sqlite")]
        let summaries = read_summaries_map(&loc.app_data_dir.join("conversation_summaries.db")).unwrap_or_default();
        let mut out = Vec::new();
        for desc in descriptors {
            if !select(&desc.conversation_id, None) {
                continue;
            }
            let child_brain = loc.app_data_dir.join("brain").join(&desc.conversation_id);
            let Some((_, child_log)) = select_brain_log(&child_brain) else {
                continue;
            };
            let Some(mut child_ref) = scan_transcript(&desc.conversation_id, &child_log, Some(&loc.app_data_dir))?
            else {
                continue;
            };
            if child_ref.cwd.is_none() {
                child_ref.cwd = desc.workspace_uris.iter().find_map(|u| file_uri_to_path(u));
            }
            #[cfg(feature = "sqlite")]
            if let Some(sum) = summaries.get(&desc.conversation_id) {
                let has_pbtxt = read_pbtxt_title(
                    &loc.app_data_dir
                        .join("annotations")
                        .join(format!("{}.pbtxt", desc.conversation_id)),
                )
                .is_some_and(|t| !t.trim().is_empty());
                apply_summary_fallback(&mut child_ref, sum, has_pbtxt);
            }
            let tool_use_id = desc
                .spawn_step_index
                .map(|step| derived_call_id("antigravity", format!("{step}.0")));
            out.push(crate::AgentRef {
                session: child_ref,
                agent_type: desc.type_name,
                description: desc.role,
                tool_use_id,
                workflow: None,
                result_status: desc.state.map(|s| s.to_ascii_lowercase()),
                result_summary: None,
            });
        }
        out.sort_by_key(|a| std::cmp::Reverse(a.session.created_at));
        Ok(out)
    }

    fn discover(&self) -> Result<crate::Discovered> {
        let _cache_scope = crate::discover_cache::read::Context::current().enter();
        #[cfg(feature = "sqlite")]
        let _sqlite_scope = super::store::sqlite::read_scope();

        let mut out = crate::Discovered::default();
        // Deduplicate by session UUID across roots, preferring higher-fidelity log ranks
        // (`transcript_full.jsonl` = 3 > `transcript.jsonl` = 2 > `overview.txt` = 1 > flat `.jsonl` = 0).
        // Earlier roots win ties (`antigravity-cli` > `antigravity` > `antigravity-ide`).
        struct Candidate {
            rank: u8,
            transcript: PathBuf,
            app_data_dir: Option<PathBuf>,
        }
        let mut candidates: BTreeMap<String, Candidate> = BTreeMap::new();
        #[cfg(feature = "sqlite")]
        let mut summaries_by_root: HashMap<PathBuf, HashMap<String, SummaryRow>> = HashMap::new();

        for root in &self.roots {
            if !root.is_dir() {
                continue;
            }
            #[cfg(feature = "sqlite")]
            {
                let summaries_db = root.join("conversation_summaries.db");
                if summaries_db.is_file() {
                    if let Some(map) = out.keep(&summaries_db, read_summaries_map(&summaries_db)) {
                        summaries_by_root.insert(root.clone(), map);
                    }
                }
            }

            let brain_root = root.join("brain");
            if brain_root.is_dir() {
                if let Some(entries) = out.keep(&brain_root, fs::read_dir(&brain_root).map_err(Into::into)) {
                    for entry in entries {
                        let Some(entry) = out.keep(&brain_root, entry.map_err(Into::into)) else {
                            continue;
                        };
                        let name = entry.file_name();
                        let Some(id) = name.to_str() else {
                            continue;
                        };
                        if id.starts_with('.') {
                            continue;
                        }
                        let brain_dir = entry.path();
                        if !brain_dir.is_dir() {
                            continue;
                        }
                        let Some((rank, log_path)) = select_brain_log(&brain_dir) else {
                            continue;
                        };
                        let replace = match candidates.get(id) {
                            None => true,
                            Some(prev) => rank > prev.rank,
                        };
                        if replace {
                            candidates.insert(
                                id.to_string(),
                                Candidate {
                                    rank,
                                    transcript: log_path,
                                    app_data_dir: Some(root.clone()),
                                },
                            );
                        }
                    }
                }
            }

            // Also support flat `*.jsonl` transcripts when `with_home` points directly at a
            // directory of exported Antigravity step logs (excluding `history.jsonl`).
            if let Some(entries) = out.keep(root, fs::read_dir(root).map_err(Into::into)) {
                for entry in entries {
                    let Some(entry) = out.keep(root, entry.map_err(Into::into)) else {
                        continue;
                    };
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    let Some(fname) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if fname == "history.jsonl" || !fname.ends_with(".jsonl") {
                        continue;
                    }
                    let id = fname.trim_end_matches(".jsonl").to_string();
                    if id.is_empty() || candidates.contains_key(&id) {
                        continue;
                    }
                    if looks_like_antigravity_file(&path) {
                        candidates.insert(
                            id,
                            Candidate {
                                rank: 0,
                                transcript: path,
                                app_data_dir: None,
                            },
                        );
                    }
                }
            }
        }

        let items: Vec<(String, Candidate)> = candidates.into_iter().collect();
        let scanned = crate::par_discovery_filter_map(items, |(id, cand)| {
            let mut reads = ReadSet::default();
            reads.record(&cand.transcript);
            if let Some(app_dir) = &cand.app_data_dir {
                reads.record(&app_dir.join("annotations").join(format!("{id}.pbtxt")));
                reads.record(
                    &app_dir
                        .join("brain")
                        .join(&id)
                        .join(".system_generated")
                        .join("subagents"),
                );
                #[cfg(feature = "sqlite")]
                record_sqlite_readset(&app_dir.join("conversations").join(format!("{id}.db")), &mut reads);
            }
            let res = crate::discover_cache::cached_scan_with_reads(&cand.transcript, reads, || {
                scan_transcript(&id, &cand.transcript, cand.app_data_dir.as_deref())
            });
            #[cfg(feature = "sqlite")]
            let res = res.map(|opt| {
                opt.map(|mut sref| {
                    if let Some(sum) = cand
                        .app_data_dir
                        .as_ref()
                        .and_then(|r| summaries_by_root.get(r))
                        .and_then(|m| m.get(&id))
                    {
                        let has_pbtxt = cand
                            .app_data_dir
                            .as_ref()
                            .and_then(|app| read_pbtxt_title(&app.join("annotations").join(format!("{id}.pbtxt"))))
                            .is_some_and(|t| !t.trim().is_empty());
                        apply_summary_fallback(&mut sref, sum, has_pbtxt);
                    }
                    sref
                })
            });
            crate::Skipped::listed(&cand.transcript, res)
        });
        for found in scanned {
            out.push(found);
        }
        Ok(out)
    }

    fn stream(&self, r: &SessionRef, opts: &Demand, sink: &mut dyn MessageSink) -> Result<Session> {
        self.transcript(r, opts, Start::Whole, sink)?
            .context("a whole read never declines")
    }

    fn transcript(
        &self,
        r: &SessionRef,
        opts: &Demand,
        from: Start<'_>,
        sink: &mut dyn MessageSink,
    ) -> Result<Option<Session>> {
        crate::stream::checked(sink, |sink| {
            let mut side_reads = ReadSet::default();
            if let Some(loc) = BrainLoc::from_transcript(&r.path) {
                let pbtxt = loc.app_data_dir.join("annotations").join(format!("{}.pbtxt", loc.id));
                side_reads.record(&pbtxt);
                sink.dependency(&pbtxt);
                #[cfg(feature = "sqlite")]
                {
                    let conv_db = loc.app_data_dir.join("conversations").join(format!("{}.db", loc.id));
                    record_sqlite_readset(&conv_db, &mut side_reads);
                    sink.dependency(&conv_db);
                }
            }
            let source = crate::json::JsonSource::capture_with_reads(&r.path, &side_reads)?;
            let input = jsonl::Read::with(source)?;
            let state = match from {
                Start::Checkpoint(cp) => match jsonl::restore::<ResumeState>(cp) {
                    Some(saved) if saved.source == r.path && cp.offset <= input.len() => saved.state,
                    _ => return Ok(None),
                },
                Start::Whole | Start::Prefix(_) => StepState::default(),
            };
            let (s, enrich) = build_session_header(r, opts);
            let mut spawns = crate::affordance::AffordanceSink::new(Harness::Antigravity, sink);
            spawns.meta(&s);
            let mut codec = StepCodec {
                s,
                opts: *opts,
                spans: opts.spans,
                enrich,
                state,
                source_path: Some(r.path.clone()),
            };
            codec
                .state
                .child_ids
                .extend(codec.enrich.known_children.iter().cloned());
            jsonl::drive(&input, from, codec, &mut spawns)
        })
    }
}

// ---------------------------------------------------------------------------
// Pure in-memory JSONL entrypoints (`ingest`, tests, wasm32)
// ---------------------------------------------------------------------------

/// Sniff whether a single JSONL line is an Antigravity Cortex step record.
pub(crate) fn is_step_record_view(view: JsonView<'_>) -> bool {
    let Ok(Some(fields)) = Fields::parse(view, &["step_index", "source", "type"]) else {
        return false;
    };
    let has_idx = fields
        .get("step_index")
        .and_then(|v| v.as_number().ok().flatten())
        .and_then(|n| n.as_u64())
        .is_some();
    if !has_idx {
        return false;
    }
    let Ok(Some(source)) = fields.string("source") else {
        return false;
    };
    let Ok(Some(ty)) = fields.string("type") else {
        return false;
    };
    is_known_source(&source)
        && matches!(
            ty.as_str(),
            "USER_INPUT" | "PLANNER_RESPONSE" | "GENERIC" | "SYSTEM_MESSAGE" | "CHECKPOINT" | "ERROR_MESSAGE"
        )
}

fn is_known_source(source: &str) -> bool {
    matches!(source, "USER_EXPLICIT" | "MODEL" | "SYSTEM" | "SYSTEM_SDK")
}

/// Parse an Antigravity JSONL transcript from memory without filesystem access.
pub fn parse_str(id: &str, text: &str, source_path: Option<PathBuf>) -> Result<Session> {
    struct Collect(Vec<Message>);
    impl MessageSink for Collect {
        fn message(&mut self, m: Message) -> Flow {
            self.0.push(m);
            Flow::Continue
        }
    }
    let mut collect = Collect(Vec::new());
    let mut s = stream_str(id, text, source_path, &Demand::RICH, &mut collect)?;
    s.messages = collect.0;
    Ok(s)
}

/// Stream an in-memory Antigravity JSONL transcript into `sink`.
pub fn stream_str(
    id: &str,
    text: &str,
    source_path: Option<PathBuf>,
    opts: &Demand,
    sink: &mut dyn MessageSink,
) -> Result<Session> {
    crate::stream::checked(sink, |sink| {
        let s = Session {
            source_path: source_path.clone(),
            ..Session::new(id, Harness::Antigravity)
        };
        let mut spawns = crate::affordance::AffordanceSink::new(Harness::Antigravity, sink);
        spawns.meta(&s);
        let mut codec = StepCodec {
            s,
            opts: *opts,
            spans: false,
            enrich: SidecarEnrich::default(),
            state: StepState::default(),
            source_path,
        };
        let mut skipped = 0u64;
        let mut stopped = false;
        jsonl::slice(text.as_bytes(), |line| {
            match line.view {
                Some(view) => {
                    if codec.record(view, line.at, &mut spawns)? == Flow::Stop {
                        stopped = true;
                        return Ok(Flow::Stop);
                    }
                }
                None => skipped += u64::from(!line.blank),
            }
            Ok(Flow::Continue)
        })?;
        super::note_skipped_lines(codec.session(), skipped);
        codec.finish(stopped, &mut spawns)
    })
}

// ---------------------------------------------------------------------------
// Layout & sidecar enrichment
// ---------------------------------------------------------------------------

struct BrainLoc {
    app_data_dir: PathBuf,
    brain_dir: PathBuf,
    id: String,
}

impl BrainLoc {
    /// Match `<appDataDir>/brain/<id>/.system_generated/logs/<filename>`.
    fn from_transcript(path: &Path) -> Option<Self> {
        let logs_dir = path.parent()?;
        if logs_dir.file_name()?.to_str()? != "logs" {
            return None;
        }
        let sys_dir = logs_dir.parent()?;
        if sys_dir.file_name()?.to_str()? != ".system_generated" {
            return None;
        }
        let brain_dir = sys_dir.parent()?;
        let id = brain_dir.file_name()?.to_str()?.to_string();
        let brain_root = brain_dir.parent()?;
        if brain_root.file_name()?.to_str()? != "brain" {
            return None;
        }
        let app_data_dir = brain_root.parent()?.to_path_buf();
        Some(BrainLoc {
            app_data_dir,
            brain_dir: brain_dir.to_path_buf(),
            id,
        })
    }
}

/// Prefer `transcript_full.jsonl` (rank 3) > `transcript.jsonl` (rank 2) > `overview.txt` (rank 1).
/// Never enters `logs/chunks/`.
fn select_brain_log(brain_dir: &Path) -> Option<(u8, PathBuf)> {
    let logs = brain_dir.join(".system_generated").join("logs");
    let full = logs.join("transcript_full.jsonl");
    if full.is_file() {
        return Some((3, full));
    }
    let compact = logs.join("transcript.jsonl");
    if compact.is_file() {
        return Some((2, compact));
    }
    let overview = logs.join("overview.txt");
    if overview.is_file() {
        return Some((1, overview));
    }
    None
}

fn looks_like_antigravity_file(path: &Path) -> bool {
    let Ok(input) = jsonl::Read::open(path) else {
        return false;
    };
    let mut matched = false;
    let _ = input.prefix(64 * 1024, |line| {
        if let Some(view) = line.view {
            if is_step_record_view(view) {
                matched = true;
            }
            return Ok(Flow::Stop);
        }
        Ok(Flow::Continue)
    });
    matched
}

#[derive(Default)]
struct SidecarEnrich {
    step_models: BTreeMap<u64, String>,
    known_children: BTreeSet<String>,
    subagent_descriptors: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
struct SubagentDescriptor {
    conversation_id: String,
    spawn_step_index: Option<u64>,
    state: Option<String>,
    type_name: Option<String>,
    role: Option<String>,
    workspace_uris: Vec<String>,
}

fn read_subagent_descriptors(brain_dir: &Path) -> Result<Vec<SubagentDescriptor>> {
    use std::io::Read as _;
    let sub_dir = brain_dir.join(".system_generated").join("subagents");
    let entries = super::side_entries(&sub_dir)?;
    let mut paths: Vec<PathBuf> = entries
        .into_iter()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json") && p.is_file())
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        let Ok(meta) = file.metadata() else {
            continue;
        };
        if !meta.file_type().is_file() || meta.len() > 1024 * 1024 {
            continue;
        }
        let mut text = String::new();
        if std::io::Read::take(file, 1024 * 1024 + 1)
            .read_to_string(&mut text)
            .is_err()
            || text.len() > 1024 * 1024
        {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(cid) = val
            .get("conversationId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
        else {
            continue;
        };
        let spawn_step_index = val.get("spawnStepIndex").and_then(Value::as_u64);
        let state = val.get("state").and_then(Value::as_str).map(str::to_owned);
        let desc_obj = val.get("subagentDescriptor");
        let type_name = desc_obj
            .and_then(|d| d.get("typeName"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let role = desc_obj
            .and_then(|d| d.get("role"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let workspace_uris = val
            .get("workspaceUris")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        out.push(SubagentDescriptor {
            conversation_id: cid,
            spawn_step_index,
            state,
            type_name,
            role,
            workspace_uris,
        });
    }
    Ok(out)
}

fn scan_transcript(id: &str, transcript: &Path, app_data_dir: Option<&Path>) -> Result<Option<SessionRef>> {
    let meta = match fs::metadata(transcript) {
        Ok(m) if m.is_file() => m,
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mtime: Option<DateTime<Utc>> = meta.modified().ok().map(Into::into);

    let mut title = app_data_dir
        .and_then(|app| read_pbtxt_title(&app.join("annotations").join(format!("{id}.pbtxt"))))
        .filter(|t| !t.trim().is_empty());
    let mut cwd: Option<PathBuf> = None;
    let mut created_at: Option<DateTime<Utc>> = None;

    if let Some(app) = app_data_dir {
        let brain_dir = app.join("brain").join(id);
        if let Ok(descs) = read_subagent_descriptors(&brain_dir) {
            cwd = descs
                .iter()
                .flat_map(|d| d.workspace_uris.iter())
                .find_map(|u| file_uri_to_path(u));
        }
        #[cfg(feature = "sqlite")]
        {
            let conv_db = app.join("conversations").join(format!("{id}.db"));
            if conv_db.is_file() {
                if let Ok(db_meta) = read_conversation_db(&conv_db) {
                    if cwd.is_none() {
                        cwd = db_meta.cwd;
                    }
                    if created_at.is_none() {
                        created_at = db_meta.created_at;
                    }
                }
            }
        }
    }

    // Peek head of transcript for `created_at` and fallback prompt title without scanning the body.
    if let Ok(input) = jsonl::Read::open(transcript) {
        let mut inspected = 0usize;
        let _ = input.prefix(64 * 1024, |line| {
            let Some(view) = line.view else {
                return Ok(Flow::Continue);
            };
            inspected += 1;
            if let Some(fields) = Fields::parse(view, &["step_index", "type", "created_at", "content"])? {
                if created_at.is_none() {
                    if let Some(ts_str) = fields.string("created_at")? {
                        created_at = super::parse_ts(&ts_str);
                    }
                }
                if title.is_none() {
                    let ty = fields.string("type")?.unwrap_or_default();
                    let idx = fields
                        .get("step_index")
                        .and_then(|v| v.as_number().ok().flatten())
                        .and_then(|n| n.as_u64());
                    if ty == "USER_INPUT" || (ty == "SYSTEM_MESSAGE" && idx == Some(0)) {
                        if let Some(raw) = fields.string("content")? {
                            let body = if ty == "SYSTEM_MESSAGE" {
                                unwrap_system_message_body(&raw).unwrap_or(&raw)
                            } else {
                                unwrap_user_request(&raw)
                            };
                            let cleaned = body.trim();
                            if !cleaned.is_empty() {
                                let mut head = crate::projection::Head::bounded(crate::projection::Head::TITLE);
                                head.push(cleaned);
                                title = Some(head.finish());
                            }
                        }
                    }
                }
            }
            if (created_at.is_some() && title.is_some()) || inspected >= 8 {
                return Ok(Flow::Stop);
            }
            Ok(Flow::Continue)
        });
    }

    Ok(Some(SessionRef {
        id: id.to_string(),
        harness: Harness::Antigravity,
        path: transcript.to_path_buf(),
        cwd,
        title,
        created_at: created_at.or(mtime),
        updated_at: mtime.or(created_at),
        message_count: None,
    }))
}

fn build_session_header(r: &SessionRef, opts: &Demand) -> (Session, SidecarEnrich) {
    let mut s = Session {
        cwd: r.cwd.clone(),
        title: r.title.clone(),
        created_at: r.created_at,
        updated_at: r.updated_at,
        source_path: Some(r.path.clone()),
        ..Session::new(r.id.clone(), Harness::Antigravity)
    };
    let mut enrich = SidecarEnrich::default();

    let Some(loc) = BrainLoc::from_transcript(&r.path) else {
        return (s, enrich);
    };

    let pbtxt_title = read_pbtxt_title(&loc.app_data_dir.join("annotations").join(format!("{}.pbtxt", loc.id)))
        .filter(|t| !t.trim().is_empty());
    #[cfg(feature = "sqlite")]
    let has_pbtxt = pbtxt_title.is_some();
    if let Some(pbtxt_title) = pbtxt_title {
        s.title = Some(pbtxt_title);
    }

    let want_extra = opts.tier >= Tier::Rich;

    if let Ok(descs) = read_subagent_descriptors(&loc.brain_dir) {
        for d in descs {
            if s.cwd.is_none() {
                s.cwd = d.workspace_uris.iter().find_map(|u| file_uri_to_path(u));
            }
            enrich.known_children.insert(d.conversation_id.clone());
            if want_extra {
                let mut entry = Map::new();
                if let Some(step) = d.spawn_step_index {
                    entry.insert("spawnStepIndex".into(), Value::Number(step.into()));
                }
                if let Some(st) = d.state {
                    entry.insert("state".into(), Value::String(st));
                }
                if let Some(tn) = d.type_name {
                    entry.insert("typeName".into(), Value::String(tn));
                }
                if let Some(role) = d.role {
                    entry.insert("role".into(), Value::String(role));
                }
                enrich
                    .subagent_descriptors
                    .insert(d.conversation_id, Value::Object(entry));
            }
        }
    }

    #[cfg(feature = "sqlite")]
    {
        let conv_db = loc.app_data_dir.join("conversations").join(format!("{}.db", loc.id));
        if conv_db.is_file() {
            if let Ok(db_meta) = read_conversation_db(&conv_db) {
                if s.cwd.is_none() {
                    s.cwd = db_meta.cwd;
                }
                if s.created_at.is_none() {
                    s.created_at = db_meta.created_at;
                }
                if let Some(branch) = db_meta.git_branch {
                    s.git = Some(GitInfo {
                        branch: Some(branch),
                        commit: None,
                        remote: None,
                    });
                }
                if let Some(git_root) = db_meta.git_root {
                    if want_extra {
                        s.harness_extra_mut(Harness::Antigravity)
                            .insert("git_root".into(), Value::String(git_root.display().to_string()));
                    }
                }
                s.model = db_meta.dominant_model;
                enrich.step_models = db_meta.step_models;
            }
        }

        let sum_db = loc.app_data_dir.join("conversation_summaries.db");
        if sum_db.is_file() {
            if let Ok(Some(sum)) = read_summary_one(&sum_db, &loc.id) {
                if !has_pbtxt {
                    if let Some(sum_title) = sum.title {
                        s.title = Some(sum_title);
                    } else if s.title.is_none() {
                        s.title = sum.preview;
                    }
                } else if s.title.is_none() {
                    s.title = sum.title.or(sum.preview);
                }
                if s.cwd.is_none() {
                    s.cwd = sum.cwd;
                }
                if s.updated_at.is_none() {
                    s.updated_at = sum.updated_at;
                }
                if let Some(parent) = sum.parent_id.filter(|p| !p.is_empty()) {
                    s.lineage.parent = Some(parent);
                }
                if let Some(agent_name) = sum.agent_name.filter(|a| !a.is_empty()) {
                    if s.lineage.parent.is_some() && s.lineage.agent_path.is_none() {
                        s.lineage.agent_path = Some(agent_name.clone());
                    }
                    if want_extra {
                        s.harness_extra_mut(Harness::Antigravity)
                            .insert("agent_name".into(), Value::String(agent_name));
                    }
                }
                if want_extra {
                    let bag = s.harness_extra_mut(Harness::Antigravity);
                    if let Some(source) = sum.source.filter(|x| !x.is_empty()) {
                        bag.insert("source".into(), Value::String(source));
                    }
                    if let Some(project_id) = sum.project_id.filter(|x| !x.is_empty()) {
                        bag.insert("project_id".into(), Value::String(project_id));
                    }
                }
            }
        }
    }

    (s, enrich)
}

// ---------------------------------------------------------------------------
// Step JSONL Codec
// ---------------------------------------------------------------------------

const STEP_KEYS: &[&str] = &[
    "step_index",
    "source",
    "type",
    "status",
    "created_at",
    "content",
    "thinking",
    "tool_calls",
    "input_tokens",
    "cache_read_tokens",
    "output_tokens",
    "error",
    "media",
    "truncated_fields",
];

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PendingCall {
    id: String,
    name: String,
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct StepState {
    pending_calls: VecDeque<PendingCall>,
    child_ids: BTreeSet<String>,
    spawned_children: BTreeMap<String, String>,
    first_ts: Option<DateTime<Utc>>,
    last_ts: Option<DateTime<Utc>>,
    inferred_parent: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ResumeState<P = PathBuf, S = StepState> {
    source: P,
    state: S,
}

struct StepCodec {
    s: Session,
    opts: Demand,
    spans: bool,
    enrich: SidecarEnrich,
    state: StepState,
    source_path: Option<PathBuf>,
}

impl jsonl::Codec for StepCodec {
    fn demand(&self) -> Demand {
        self.opts
    }

    fn session(&mut self) -> &mut Session {
        &mut self.s
    }

    fn state(&self) -> impl serde::Serialize + '_ {
        ResumeState {
            source: self.source_path.as_deref().unwrap_or_else(|| Path::new("")),
            state: &self.state,
        }
    }

    fn record(&mut self, view: JsonView<'_>, _at: u64, sink: &mut dyn MessageSink) -> Result<Flow> {
        let Some(fields) = Fields::parse(view, STEP_KEYS)? else {
            return Ok(Flow::Continue);
        };
        let Some(ty) = fields.string("type")? else {
            return Ok(Flow::Continue);
        };
        let step_index = fields
            .get("step_index")
            .and_then(|v| v.as_number().ok().flatten())
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let source = fields.string("source")?.unwrap_or_default();
        let status = fields.string("status")?;
        let ts = fields.string("created_at")?.as_deref().and_then(super::parse_ts);

        if let Some(t) = ts {
            if self.state.first_ts.is_none() {
                self.state.first_ts = Some(t);
            }
            self.state.last_ts = Some(t);
        }

        // Edge case A: if a turn was interrupted/cancelled (`len(GENERIC) < len(tool_calls)`),
        // clear any stale pending calls as soon as a non-GENERIC step arrives so later turns
        // never mis-pair.
        if ty != "GENERIC" {
            self.state.pending_calls.clear();
        }

        let want_extra = self.opts.tier >= Tier::Rich;
        match ty.as_str() {
            "USER_INPUT" => {
                let mut m = Message::of_kind(Role::User, MessageKind::Prompt, Origin::Human);
                m.id = Some(step_index.to_string());
                m.timestamp = ts;
                push_nonempty_text(&mut m.content, fields.get("content"), self.spans)?;
                push_media_blocks(&mut m.content, fields.get("media"))?;
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                Ok(sink.message(m))
            }
            "PLANNER_RESPONSE" => {
                let mut m = Message::of_kind(Role::Assistant, MessageKind::Reply, Origin::Model);
                m.id = Some(step_index.to_string());
                m.timestamp = ts;
                m.model = self
                    .enrich
                    .step_models
                    .get(&step_index)
                    .cloned()
                    .or_else(|| self.s.model.clone());

                if let Some(thinking_view) = fields.get("thinking") {
                    if let Some(text) = nonempty_view_text(thinking_view, self.spans)? {
                        m.content.push(Block::Thinking {
                            text,
                            signature: None,
                            encrypted: None,
                            redacted: false,
                        });
                    }
                }

                push_nonempty_text(&mut m.content, fields.get("content"), self.spans)?;

                if let Some(calls) = fields.get("tool_calls").map(JsonView::array).transpose()?.flatten() {
                    for (k, item) in calls.enumerate() {
                        let item = item?;
                        let name = item
                            .get("name")?
                            .map(JsonView::string)
                            .transpose()?
                            .flatten()
                            .unwrap_or_default();
                        let input = item
                            .get("args")?
                            .map(|v| v.argument(self.spans))
                            .transpose()?
                            .unwrap_or_else(|| Json::Owned(json!({})));
                        let call_id = derived_call_id("antigravity", format!("{step_index}.{k}"));
                        self.state.pending_calls.push_back(PendingCall {
                            id: call_id.clone(),
                            name: name.clone(),
                        });
                        m.content.push(Block::ToolUse {
                            id: call_id,
                            name,
                            input,
                            namespace: None,
                        });
                    }
                }

                m.usage = parse_step_usage(&fields)?;
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                Ok(sink.message(m))
            }
            "GENERIC" => {
                let mut m = Message::of_kind(Role::Tool, MessageKind::ToolResult, Origin::Harness);
                m.id = Some(step_index.to_string());
                m.timestamp = ts;

                let (tool_use_id, tool_name) = match self.state.pending_calls.pop_front() {
                    Some(call) => (call.id, Some(call.name)),
                    None => (derived_call_id("antigravity", step_index), None),
                };

                let is_error = matches!(status.as_deref(), Some("ERROR" | "INVALID"));
                let norm_status = status.as_deref().map(normalize_step_status);

                // Inspect a bounded 64 KiB prefix of `content` for persisted-output paths and
                // `invoke_subagent` child conversationIds so multi-megabyte tool outputs stay in
                // their zero-copy source spans without truncating long preamble headers.
                const STEP_PEEK_BYTES: usize = 64 * 1024;
                let head = fields
                    .get("content")
                    .map(|v| peek_view_prefix(v, STEP_PEEK_BYTES))
                    .transpose()?
                    .unwrap_or_default();

                let details = extract_persisted_output_path(&head).map(|path| {
                    json!({
                        "persistedOutput": { "path": path }
                    })
                });

                if matches!(tool_name.as_deref(), Some("invoke_subagent")) {
                    if let Some(child_id) = extract_spawned_conversation_id(&head) {
                        self.state.child_ids.insert(child_id.clone());
                        self.state.spawned_children.insert(child_id, tool_use_id.clone());
                    }
                }

                let error_text = fields.string("error")?.filter(|e| !e.is_empty());
                let content = match error_text {
                    Some(err) => {
                        let body = fields.string("content")?.unwrap_or_default();
                        if body.is_empty() {
                            Text::Inline(err)
                        } else if body.contains(&err) {
                            fields
                                .get("content")
                                .map(|v| v.text(self.spans))
                                .transpose()?
                                .flatten()
                                .unwrap_or_else(|| Text::Inline(body))
                        } else {
                            Text::Inline(format!("{body}\n{err}"))
                        }
                    }
                    None => fields
                        .get("content")
                        .map(|v| v.text(self.spans))
                        .transpose()?
                        .flatten()
                        .unwrap_or_else(|| Text::Inline(String::new())),
                };

                m.content.push(Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    tool_name,
                    status: norm_status,
                    details,
                });
                push_media_blocks(&mut m.content, fields.get("media"))?;
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                Ok(sink.message(m))
            }
            "SYSTEM_MESSAGE" => {
                const SYS_PEEK_BYTES: usize = 64 * 1024;
                let head = fields
                    .get("content")
                    .map(|v| peek_view_prefix(v, SYS_PEEK_BYTES))
                    .transpose()?
                    .unwrap_or_default();
                let sender = extract_system_message_sender(&head);

                let (role, kind, origin) = if step_index == 0
                    && sender
                        .as_deref()
                        .is_some_and(|s_id| looks_like_uuid(s_id) && s_id != self.s.id)
                {
                    let parent = sender.clone().unwrap();
                    if self.state.inferred_parent.is_none() {
                        self.state.inferred_parent = Some(parent);
                    }
                    (Role::User, MessageKind::Prompt, Origin::Subagent)
                } else if sender
                    .as_deref()
                    .is_some_and(|s_id| self.state.child_ids.contains(s_id))
                {
                    (Role::User, MessageKind::SubagentReturn, Origin::Subagent)
                } else if sender
                    .as_deref()
                    .is_some_and(|s_id| s_id.contains("/task-") || s_id.contains("timer"))
                {
                    (Role::System, MessageKind::InjectedContext, Origin::Scheduler)
                } else {
                    (Role::System, MessageKind::InjectedContext, Origin::Harness)
                };

                let mut m = Message::of_kind(role, kind, origin);
                m.id = Some(step_index.to_string());
                m.timestamp = ts;
                push_nonempty_text(&mut m.content, fields.get("content"), self.spans)?;
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                if want_extra {
                    if let Some(sender) = sender {
                        m.harness_extra_mut(Harness::Antigravity)
                            .insert("sender".into(), Value::String(sender));
                    }
                }
                Ok(sink.message(m))
            }
            "CHECKPOINT" => {
                let mut boundary = Message::synthesized(MessageKind::CompactionBoundary, Origin::Harness);
                boundary.timestamp = ts;
                if sink.message(boundary) == Flow::Stop {
                    return Ok(Flow::Stop);
                }
                let mut m = Message::of_kind(Role::System, MessageKind::CompactionSummary, Origin::Harness);
                m.id = Some(step_index.to_string());
                m.timestamp = ts;
                push_nonempty_text(&mut m.content, fields.get("content"), self.spans)?;
                m.usage = parse_step_usage(&fields)?;
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                Ok(sink.message(m))
            }
            "ERROR_MESSAGE" => {
                let mut m = Message::of_kind(Role::System, MessageKind::Error, Origin::Harness);
                m.id = Some(step_index.to_string());
                m.timestamp = ts;
                let err = fields.string("error")?.unwrap_or_default();
                let body = fields.string("content")?.unwrap_or_default();
                let combined = match (err.is_empty(), body.is_empty()) {
                    (false, false) if body != err && !body.contains(&err) => Some(format!("{err}\n{body}")),
                    (false, _) => Some(err),
                    (true, false) => Some(body),
                    (true, true) => None,
                };
                if let Some(text) = combined {
                    m.content.push(Block::Text {
                        text: Text::Inline(text),
                    });
                }
                attach_step_extras(&mut m, &fields, step_index, &source, status.as_deref(), want_extra)?;
                Ok(sink.message(m))
            }
            _ => {
                if self.opts.tier == Tier::Complete {
                    let mut m = Message::of_kind(Role::System, MessageKind::Carrier, Origin::Harness);
                    m.id = Some(step_index.to_string());
                    m.timestamp = ts;
                    m.harness_extra_mut(Harness::Antigravity)
                        .insert("record_type".into(), Value::String(ty));
                    m.extra.insert(CARRIER_KEY.into(), view.resolve()?.into_owned());
                    return Ok(sink.message(m));
                }
                Ok(Flow::Continue)
            }
        }
    }

    fn finish(mut self, _stopped: bool, _sink: &mut dyn MessageSink) -> Result<Session> {
        if self.s.created_at.is_none() {
            self.s.created_at = self.state.first_ts;
        }
        if let Some(last) = self.state.last_ts {
            if self.s.updated_at.is_none_or(|prev| last > prev) {
                self.s.updated_at = Some(last);
            }
        }
        if self.s.lineage.parent.is_none() {
            self.s.lineage.parent = self.state.inferred_parent;
        }
        if self.opts.tier >= Tier::Rich
            && (!self.enrich.subagent_descriptors.is_empty() || !self.state.spawned_children.is_empty())
        {
            let mut merged = self.enrich.subagent_descriptors;
            for (cid, call_id) in self.state.spawned_children {
                let entry = merged.entry(cid).or_insert_with(|| Value::Object(Map::new()));
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert("call_id".into(), Value::String(call_id));
                }
            }
            self.s
                .harness_extra_mut(Harness::Antigravity)
                .insert("subagents".into(), Value::Object(merged.into_iter().collect()));
        }
        Ok(self.s)
    }
}

fn normalize_step_status(status: &str) -> String {
    match status {
        "DONE" => "completed".to_string(),
        "RUNNING" => "running".to_string(),
        "ERROR" => "error".to_string(),
        "INVALID" => "invalid".to_string(),
        "CANCELED" => "canceled".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

fn parse_step_usage(fields: &Fields<'_>) -> Result<Option<Usage>> {
    let u64_field = |k: &str| -> Option<u64> {
        fields
            .get(k)
            .and_then(|v| v.as_number().ok().flatten())
            .and_then(|n| n.as_u64())
    };
    let input_tokens = u64_field("input_tokens");
    let output_tokens = u64_field("output_tokens");
    let cache_read_tokens = u64_field("cache_read_tokens");
    let usage = Usage {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        ..Default::default()
    };
    Ok((!usage.is_empty()).then_some(usage))
}

fn nonempty_view_text(view: JsonView<'_>, spans: bool) -> Result<Option<Text>> {
    let mut non_empty = false;
    let is_str = view.for_each_str_chunk(|chunk| {
        if !chunk.is_empty() {
            non_empty = true;
            return Err(crate::stream::Stop.into());
        }
        Ok(())
    });
    match is_str {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        Err(e) if e.is::<crate::stream::Stop>() => {}
        Err(e) => return Err(e),
    }
    if !non_empty {
        return Ok(None);
    }
    view.text(spans)
}

fn push_nonempty_text(blocks: &mut Vec<Block>, view: Option<JsonView<'_>>, spans: bool) -> Result<()> {
    if let Some(view) = view {
        if let Some(text) = nonempty_view_text(view, spans)? {
            blocks.push(Block::Text { text });
        }
    }
    Ok(())
}

fn push_media_blocks(blocks: &mut Vec<Block>, media_view: Option<JsonView<'_>>) -> Result<()> {
    let Some(items) = media_view.map(JsonView::array).transpose()?.flatten() else {
        return Ok(());
    };
    for item in items {
        let item = item?;
        let mime = item.get("mime_type")?.map(JsonView::string).transpose()?.flatten();
        let uri = item.get("uri")?.map(JsonView::string).transpose()?.flatten();
        let Some(raw_uri) = uri.filter(|u| !u.is_empty()) else {
            continue;
        };
        let resolved = file_uri_to_path(&raw_uri)
            .map(|p| p.display().to_string())
            .unwrap_or(raw_uri);
        if mime.as_deref().is_some_and(|m| m.starts_with("image/")) {
            blocks.push(Block::Image {
                media_type: mime,
                data_ref: Some(resolved),
            });
        } else {
            blocks.push(Block::File {
                mime,
                path: Some(resolved),
                source: None,
            });
        }
    }
    Ok(())
}

fn attach_step_extras(
    m: &mut Message,
    fields: &Fields<'_>,
    step_index: u64,
    source: &str,
    status: Option<&str>,
    want_extra: bool,
) -> Result<()> {
    if !want_extra {
        return Ok(());
    }
    let bag = m.harness_extra_mut(Harness::Antigravity);
    bag.insert("step_index".into(), Value::Number(step_index.into()));
    if !source.is_empty() && is_known_source(source) {
        bag.insert("source".into(), Value::String(source.to_string()));
    }
    if let Some(st) = status {
        bag.insert("status".into(), Value::String(st.to_string()));
    }
    if let Some(trunc) = fields.value("truncated_fields")? {
        if trunc.as_array().is_some_and(|a| !a.is_empty()) {
            bag.insert("truncated_fields".into(), trunc);
        }
    }
    Ok(())
}

fn peek_view_prefix(view: JsonView<'_>, max_bytes: usize) -> Result<String> {
    let mut out = String::new();
    let res = view.for_each_str_chunk(|chunk| {
        let rem = max_bytes.saturating_sub(out.len());
        if rem == 0 {
            return Err(crate::stream::Stop.into());
        }
        if chunk.len() <= rem {
            out.push_str(chunk);
        } else {
            let end = chunk.floor_char_boundary(rem);
            out.push_str(&chunk[..end]);
            return Err(crate::stream::Stop.into());
        }
        if out.len() >= max_bytes {
            return Err(crate::stream::Stop.into());
        }
        Ok(())
    });
    match res {
        Ok(_) => Ok(out),
        Err(e) if e.is::<crate::stream::Stop>() => Ok(out),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Text & sidecar parsing helpers
// ---------------------------------------------------------------------------

fn extract_persisted_output_path(head: &str) -> Option<String> {
    const MARKER: &str = "You can find the full output in a file at ";
    let idx = head.find(MARKER)?;
    let rest = &head[idx + MARKER.len()..];
    let token = rest.lines().next()?.trim().trim_end_matches('.');
    if token.is_empty() {
        return None;
    }
    Some(
        file_uri_to_path(token)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| token.to_string()),
    )
}

fn find_conversation_id_in_value(val: &Value) -> Option<String> {
    match val {
        Value::Object(map) => {
            if let Some(id) = map
                .get("conversationId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                return Some(id.to_owned());
            }
            for child in map.values() {
                if let Some(id) = find_conversation_id_in_value(child) {
                    return Some(id);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(find_conversation_id_in_value),
        _ => None,
    }
}

fn extract_spawned_conversation_id(head: &str) -> Option<String> {
    let trimmed = head.trim();
    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
        if let Some(id) = find_conversation_id_in_value(&val) {
            return Some(id);
        }
    }
    // Tool outputs often prefix or suffix JSON objects with human-readable status lines:
    // stream-parse any top-level JSON object embedded in `head` before falling back to a
    // JSON-decoded string scan if the object was truncated at the 64 KiB peek boundary.
    for (idx, ch) in head.char_indices() {
        if ch == '{' {
            let mut stream = serde_json::Deserializer::from_str(&head[idx..]).into_iter::<Value>();
            if let Some(Ok(val)) = stream.next() {
                if let Some(id) = find_conversation_id_in_value(&val) {
                    return Some(id);
                }
            }
        }
    }
    // Truncated JSON fallback: decode the JSON string value properly with serde_json so escapes
    // (`\"`, `\\`, `\uXXXX`) are handled and prose mentions without `"conversationId": "..."` fail.
    const KEY: &str = "\"conversationId\"";
    let mut search = head;
    while let Some(idx) = search.find(KEY) {
        let after_key = &search[idx + KEY.len()..];
        if let Some(after_colon) = after_key.trim_start().strip_prefix(':') {
            let val_start = after_colon.trim_start();
            if val_start.starts_with('"') {
                let mut stream = serde_json::Deserializer::from_str(val_start).into_iter::<String>();
                if let Some(Ok(id)) = stream.next() {
                    if !id.is_empty() {
                        return Some(id);
                    }
                }
            }
        }
        search = after_key;
    }
    None
}

fn extract_system_message_sender(head: &str) -> Option<String> {
    let msg_idx = head.find("[Message]")?;
    let line = head[msg_idx..].lines().next()?;
    for part in line.split_whitespace() {
        if let Some(sender) = part.strip_prefix("sender=") {
            if !sender.is_empty() {
                return Some(sender.to_string());
            }
        }
    }
    None
}

fn unwrap_system_message_body(text: &str) -> Option<&str> {
    let open = text.find("<SYSTEM_MESSAGE>")?;
    let inner = &text[open + "<SYSTEM_MESSAGE>".len()..];
    let inner = inner.strip_suffix("</SYSTEM_MESSAGE>").unwrap_or_else(|| {
        inner
            .rfind("</SYSTEM_MESSAGE>")
            .map(|end| &inner[..end])
            .unwrap_or(inner)
    });
    let inner = inner.trim();
    if let Some(rest) = inner.strip_prefix("[Message]") {
        if let Some(c_idx) = rest.find(" content=") {
            return Some(rest[c_idx + " content=".len()..].trim());
        }
    }
    Some(inner)
}

fn unwrap_user_request(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(inner) = trimmed.strip_prefix("<USER_REQUEST>") {
        let inner = inner.strip_suffix("</USER_REQUEST>").unwrap_or(inner);
        return inner.trim();
    }
    trimmed
}

fn looks_like_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 8 | 13 | 18 | 23) || b.is_ascii_hexdigit())
}

pub(crate) fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if rest.is_empty() {
        return None;
    }
    // Windows `file:///C:/...` -> `C:/...`
    let path_part = if rest.len() >= 3
        && rest.as_bytes()[0] == b'/'
        && rest.as_bytes()[1].is_ascii_alphabetic()
        && rest.as_bytes()[2] == b':'
    {
        &rest[1..]
    } else {
        rest
    };
    Some(PathBuf::from(percent_decode(path_part)))
}

fn percent_decode(input: &str) -> String {
    if !input.contains('%') {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse `annotations/<uuid>.pbtxt` (`title: "..."` with C/textproto escapes).
fn read_pbtxt_title(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    parse_pbtxt_title(&text)
}

fn parse_pbtxt_title(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("title:") else {
            continue;
        };
        let rest = rest.trim();
        let Some(quoted) = rest.strip_prefix('"') else {
            continue;
        };
        let mut bytes = Vec::with_capacity(quoted.len());
        let raw = quoted.as_bytes();
        let mut i = 0;
        while i < raw.len() {
            match raw[i] {
                b'"' => break,
                b'\\' if i + 1 < raw.len() => {
                    i += 1;
                    match raw[i] {
                        b'n' => bytes.push(b'\n'),
                        b'r' => bytes.push(b'\r'),
                        b't' => bytes.push(b'\t'),
                        b'\\' => bytes.push(b'\\'),
                        b'"' => bytes.push(b'"'),
                        b'\'' => bytes.push(b'\''),
                        b'x' if i + 2 < raw.len() => {
                            if let (Some(hi), Some(lo)) = (hex_val(raw[i + 1]), hex_val(raw[i + 2])) {
                                bytes.push((hi << 4) | lo);
                                i += 2;
                            }
                        }
                        b'0'..=b'7' => {
                            let mut val = raw[i] - b'0';
                            for _ in 0..2 {
                                if i + 1 < raw.len() && (b'0'..=b'7').contains(&raw[i + 1]) {
                                    i += 1;
                                    val = (val << 3) | (raw[i] - b'0');
                                } else {
                                    break;
                                }
                            }
                            bytes.push(val);
                        }
                        other => bytes.push(other),
                    }
                }
                ch => bytes.push(ch),
            }
            i += 1;
        }
        return String::from_utf8(bytes).ok();
    }
    None
}

// ---------------------------------------------------------------------------
// SQLite & Protobuf sidecar readers (`#[cfg(feature = "sqlite")]`)
// ---------------------------------------------------------------------------

#[cfg(feature = "sqlite")]
fn record_sqlite_deps(db: &Path, out: &mut Vec<PathBuf>) {
    out.push(db.to_path_buf());
    for suffix in ["-wal", "-journal"] {
        let mut os = db.as_os_str().to_os_string();
        os.push(suffix);
        out.push(PathBuf::from(os));
    }
}

#[cfg(feature = "sqlite")]
fn record_sqlite_readset(db: &Path, reads: &mut ReadSet) {
    reads.record(db);
    for suffix in ["-wal", "-journal"] {
        let mut os = db.as_os_str().to_os_string();
        os.push(suffix);
        reads.record(&PathBuf::from(os));
    }
}

#[cfg(feature = "sqlite")]
#[derive(Debug, Default, Clone)]
struct SummaryRow {
    title: Option<String>,
    preview: Option<String>,
    cwd: Option<PathBuf>,
    parent_id: Option<String>,
    agent_name: Option<String>,
    source: Option<String>,
    project_id: Option<String>,
    updated_at: Option<DateTime<Utc>>,
}

#[cfg(feature = "sqlite")]
fn apply_summary_fallback(sref: &mut SessionRef, sum: &SummaryRow, has_pbtxt_title: bool) {
    if !has_pbtxt_title {
        if let Some(sum_title) = sum.title.as_ref().filter(|s| !s.trim().is_empty()) {
            sref.title = Some(sum_title.clone());
        } else if sref.title.is_none() {
            sref.title = sum.preview.clone().filter(|s| !s.trim().is_empty());
        }
    } else if sref.title.is_none() {
        sref.title = sum
            .title
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| sum.preview.clone().filter(|s| !s.trim().is_empty()));
    }
    if sref.cwd.is_none() {
        sref.cwd.clone_from(&sum.cwd);
    }
    if sref.updated_at.is_none() {
        sref.updated_at = sum.updated_at;
    }
}

#[cfg(feature = "sqlite")]
fn summary_columns_sql(conn: &rusqlite::Connection) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('conversation_summaries')")?;
    let cols: BTreeSet<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<BTreeSet<_>, _>>()?;
    if !cols.contains("conversation_id") {
        return Ok(None);
    }
    let pick = |name: &str| {
        if cols.contains(name) {
            name.to_string()
        } else {
            format!("NULL AS {name}")
        }
    };
    Ok(Some(format!(
        "SELECT conversation_id, {}, {}, {}, {}, {}, {}, {}, {} FROM conversation_summaries",
        pick("title"),
        pick("preview"),
        pick("workspace_uris"),
        pick("parent_conversation_id"),
        pick("agent_name"),
        pick("source"),
        pick("project_id"),
        pick("last_modified_time"),
    )))
}

#[cfg(feature = "sqlite")]
fn decode_summary_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, SummaryRow)> {
    let id: String = row.get(0)?;
    let title: Option<String> = row.get(1)?;
    let preview: Option<String> = row.get(2)?;
    let workspace_uris: Option<String> = row.get(3)?;
    let parent_id: Option<String> = row.get(4)?;
    let agent_name: Option<String> = row.get(5)?;
    let source: Option<String> = row.get(6)?;
    let project_id: Option<String> = row.get(7)?;
    let last_modified: Option<String> = row.get(8)?;

    let cwd = workspace_uris.as_deref().and_then(first_workspace_uri_path);
    let updated_at = last_modified.as_deref().and_then(parse_sqlite_ts);
    Ok((
        id,
        SummaryRow {
            title: title.filter(|s| !s.trim().is_empty()),
            preview: preview.filter(|s| !s.trim().is_empty()),
            cwd,
            parent_id: parent_id.filter(|s| !s.trim().is_empty()),
            agent_name: agent_name.filter(|s| !s.trim().is_empty()),
            source: source.filter(|s| !s.trim().is_empty()),
            project_id: project_id.filter(|s| !s.trim().is_empty()),
            updated_at,
        },
    ))
}

#[cfg(feature = "sqlite")]
fn read_summaries_map(db_path: &Path) -> Result<HashMap<String, SummaryRow>> {
    super::store::sqlite::read(db_path, |conn, _reads| {
        let Some(select_sql) = summary_columns_sql(conn)? else {
            return Ok(HashMap::new());
        };
        let mut stmt = conn.prepare(&select_sql)?;
        let rows = stmt.query_map([], decode_summary_row)?;
        let mut map = HashMap::new();
        for item in rows {
            let (id, sum) = item?;
            map.insert(id, sum);
        }
        Ok(map)
    })
}

#[cfg(feature = "sqlite")]
fn read_summary_one(db_path: &Path, id: &str) -> Result<Option<SummaryRow>> {
    use rusqlite::OptionalExtension as _;
    super::store::sqlite::read(db_path, |conn, _reads| {
        let Some(select_sql) = summary_columns_sql(conn)? else {
            return Ok(None);
        };
        let sql = format!("{select_sql} WHERE conversation_id = ?1");
        let row = conn
            .query_row(&sql, [id], decode_summary_row)
            .optional()?
            .map(|(_, sum)| sum);
        Ok(row)
    })
}

#[cfg(feature = "sqlite")]
fn first_workspace_uri_path(raw: &str) -> Option<PathBuf> {
    if let Ok(uris) = serde_json::from_str::<Vec<String>>(raw) {
        return uris.iter().find_map(|u| file_uri_to_path(u));
    }
    file_uri_to_path(raw.trim())
}

#[cfg(feature = "sqlite")]
fn parse_sqlite_ts(s: &str) -> Option<DateTime<Utc>> {
    super::parse_ts(s).or_else(|| {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f"))
            .ok()
            .map(|ndt| ndt.and_utc())
    })
}

#[cfg(feature = "sqlite")]
#[derive(Default)]
struct ConvDbMeta {
    step_models: BTreeMap<u64, String>,
    dominant_model: Option<String>,
    cwd: Option<PathBuf>,
    git_root: Option<PathBuf>,
    git_branch: Option<String>,
    created_at: Option<DateTime<Utc>>,
}

#[cfg(feature = "sqlite")]
fn read_conversation_db(db_path: &Path) -> Result<ConvDbMeta> {
    use rusqlite::OptionalExtension as _;
    super::store::sqlite::read(db_path, |conn, _reads| {
        let mut out = ConvDbMeta::default();
        let has_table = |name: &str| -> Result<bool> {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                    [name],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        };

        if has_table("gen_metadata")? {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            let mut first_model: Option<String> = None;
            let mut stmt = conn.prepare("SELECT data FROM gen_metadata ORDER BY idx ASC")?;
            let blobs = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
            for blob in blobs {
                let bytes = blob?;
                if let Some((steps, model)) = parse_gen_metadata_blob(&bytes) {
                    if first_model.is_none() {
                        first_model = Some(model.clone());
                    }
                    let weight = steps.len().max(1);
                    *counts.entry(model.clone()).or_default() += weight;
                    for step in steps {
                        out.step_models.insert(step, model.clone());
                    }
                }
            }
            out.dominant_model = counts
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(m, _)| m)
                .or(first_model);
        }

        if has_table("trajectory_metadata_blob")? {
            let blob: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT data FROM trajectory_metadata_blob WHERE id = 'main' LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(bytes) = blob {
                parse_trajectory_metadata_blob(&bytes, &mut out);
            }
        }
        Ok(out)
    })
}

#[cfg(feature = "sqlite")]
fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut val = 0u64;
    let mut shift = 0u32;
    while *pos < buf.len() && shift < 64 {
        let byte = buf[*pos];
        *pos += 1;
        val |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(val);
        }
        shift += 7;
    }
    None
}

#[cfg(feature = "sqlite")]
enum WireField<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Other,
}

#[cfg(feature = "sqlite")]
fn next_proto_field<'a>(buf: &'a [u8], pos: &mut usize) -> Option<(u64, WireField<'a>)> {
    if *pos >= buf.len() {
        return None;
    }
    let tag = read_varint(buf, pos)?;
    let field_no = tag >> 3;
    if field_no == 0 {
        return None;
    }
    match tag & 0x07 {
        0 => Some((field_no, WireField::Varint(read_varint(buf, pos)?))),
        1 => {
            *pos = pos.checked_add(8)?;
            (*pos <= buf.len()).then_some((field_no, WireField::Other))
        }
        2 => {
            let len = usize::try_from(read_varint(buf, pos)?).ok()?;
            let end = pos.checked_add(len)?;
            if end > buf.len() {
                return None;
            }
            let slice = &buf[*pos..end];
            *pos = end;
            Some((field_no, WireField::Bytes(slice)))
        }
        5 => {
            *pos = pos.checked_add(4)?;
            (*pos <= buf.len()).then_some((field_no, WireField::Other))
        }
        _ => None,
    }
}

/// `gen_metadata.data`: field `2` = Cortex `step_index` (packed or repeated varints),
/// field `1.19` = model slug UTF-8 string (e.g. `"gemini-4-argon"`).
#[cfg(feature = "sqlite")]
fn parse_gen_metadata_blob(buf: &[u8]) -> Option<(Vec<u64>, String)> {
    let mut pos = 0usize;
    let mut steps = Vec::new();
    let mut model: Option<String> = None;
    while let Some((field_no, val)) = next_proto_field(buf, &mut pos) {
        match (field_no, val) {
            (2, WireField::Varint(step)) => steps.push(step),
            (2, WireField::Bytes(packed)) => {
                let mut p = 0usize;
                while let Some(step) = read_varint(packed, &mut p) {
                    steps.push(step);
                }
            }
            (1, WireField::Bytes(sub)) => {
                let mut sp = 0usize;
                while let Some((sf, sval)) = next_proto_field(sub, &mut sp) {
                    if let (19, WireField::Bytes(raw_str)) = (sf, sval) {
                        if let Ok(s) = std::str::from_utf8(raw_str) {
                            let trimmed = s.trim();
                            if !trimmed.is_empty() {
                                model = Some(trimmed.to_string());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Some((steps, model?))
}

/// `trajectory_metadata_blob.data`:
/// - field `1.1` = workspace `file://` URI (`cwd`)
/// - field `1.2` = git root `file://` URI
/// - field `1.4` = git branch
/// - field `2` (`google.protobuf.Timestamp`: `2.1` seconds, `2.2` nanos) = `created_at`
#[cfg(feature = "sqlite")]
fn parse_trajectory_metadata_blob(buf: &[u8], out: &mut ConvDbMeta) {
    let mut pos = 0usize;
    while let Some((field_no, val)) = next_proto_field(buf, &mut pos) {
        match (field_no, val) {
            (1, WireField::Bytes(ws)) => {
                let mut wp = 0usize;
                while let Some((wf, wval)) = next_proto_field(ws, &mut wp) {
                    if let WireField::Bytes(raw) = wval {
                        let Ok(s) = std::str::from_utf8(raw) else {
                            continue;
                        };
                        let s = s.trim();
                        if s.is_empty() {
                            continue;
                        }
                        match wf {
                            1 if out.cwd.is_none() => {
                                out.cwd = file_uri_to_path(s).or_else(|| Some(PathBuf::from(s)));
                            }
                            2 if out.git_root.is_none() => {
                                out.git_root = file_uri_to_path(s).or_else(|| Some(PathBuf::from(s)));
                            }
                            4 if out.git_branch.is_none() => {
                                out.git_branch = Some(s.to_string());
                            }
                            _ => {}
                        }
                    }
                }
            }
            (2, WireField::Bytes(ts_bytes)) => {
                let mut tp = 0usize;
                let mut secs: Option<i64> = None;
                let mut nanos: u32 = 0;
                while let Some((tf, tval)) = next_proto_field(ts_bytes, &mut tp) {
                    if let WireField::Varint(v) = tval {
                        match tf {
                            1 => secs = i64::try_from(v).ok(),
                            2 => nanos = u32::try_from(v).unwrap_or(0),
                            _ => {}
                        }
                    }
                }
                if let Some(secs) = secs.filter(|&s| s > 0) {
                    out.created_at = DateTime::from_timestamp(secs, nanos);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::Parse as _;

    const PARENT_ID: &str = "11111111-2222-4333-8444-555555555555";
    const CHILD_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    #[test]
    fn parses_steps_tools_denials_compaction_and_subagents() {
        let jsonl = [
            serde_json::to_string(&json!({
                "step_index": 0,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "status": "DONE",
                "created_at": "2026-11-11T10:00:00Z",
                "content": "<USER_REQUEST>\nInspect src/lib.rs and delegate research.\n</USER_REQUEST>",
                "media": [{"mime_type": "image/png", "uri": "file:///workspace/demo/screen.png"}]
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 2,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "created_at": "2026-11-11T10:00:02Z",
                "thinking": "Let me read the file and spawn a helper.",
                "content": "Checking files and spawning a researcher.",
                "tool_calls": [
                    {"name": "read_file", "args": {"Path": "/workspace/demo/src/lib.rs"}},
                    {"name": "write_file", "args": {"Path": "/etc/blocked", "Content": "nope"}},
                    {"name": "invoke_subagent", "args": {"TypeName": "research", "Role": "Repo Researcher", "Prompt": "Audit exports"}}
                ],
                "input_tokens": 1200,
                "cache_read_tokens": 400,
                "output_tokens": 95
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 3,
                "source": "MODEL",
                "type": "GENERIC",
                "status": "DONE",
                "created_at": "2026-11-11T10:00:03Z",
                "content": "The tool's output was truncated due to being too long. You can find the full output in a file at file:///workspace/demo/.system_generated/steps/3/output.txt\nCreated At: 2026-11-11T10:00:03Z\npub fn demo() {}"
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 4,
                "source": "MODEL",
                "type": "GENERIC",
                "status": "ERROR",
                "created_at": "2026-11-11T10:00:04Z",
                "content": "Created At: 2026-11-11T10:00:04Z",
                "error": "permission check failed for write_file on /etc/blocked"
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 5,
                "source": "MODEL",
                "type": "GENERIC",
                "status": "DONE",
                "created_at": "2026-11-11T10:00:05Z",
                "content": format!("Created At: 2026-11-11T10:00:05Z\nCreated the following subagent:\n{{\n  \"conversationId\": \"{CHILD_ID}\"\n}}")
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 8,
                "source": "SYSTEM",
                "type": "SYSTEM_MESSAGE",
                "status": "DONE",
                "created_at": "2026-11-11T10:00:10Z",
                "content": format!("Created At: 2026-11-11T10:00:10Z\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-11T10:00:10Z sender={CHILD_ID} priority=HIGH content=Audit complete: 3 exports.\n</SYSTEM_MESSAGE>")
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 9,
                "source": "SYSTEM",
                "type": "CHECKPOINT",
                "status": "DONE",
                "created_at": "2026-11-11T10:01:00Z",
                "content": "# Resuming from a compaction\nSummary of prior steps.",
                "input_tokens": 800,
                "cache_read_tokens": 100,
                "output_tokens": 40
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 10,
                "source": "SYSTEM",
                "type": "ERROR_MESSAGE",
                "status": "ERROR",
                "created_at": "2026-11-11T10:01:05Z",
                "error": "transient upstream timeout"
            }))
            .unwrap(),
        ]
        .join("\n");

        let s = parse_str(PARENT_ID, &jsonl, None).unwrap();
        crate::harness::assert_no_flat_keys(&s);
        assert_eq!(s.harness, Harness::Antigravity);
        assert_eq!(s.messages.len(), 9); // 8 records + 1 synthetic CompactionBoundary

        // Step 0: User prompt + image attachment
        assert_eq!(
            (s.messages[0].role, s.messages[0].kind),
            (Role::User, MessageKind::Prompt)
        );
        assert!(
            matches!(&s.messages[0].content[1], Block::Image { data_ref: Some(p), .. } if p == "/workspace/demo/screen.png")
        );

        // Step 2: Planner response refined to SubagentSpawn by AffordanceSink
        assert_eq!(
            (s.messages[1].role, s.messages[1].kind),
            (Role::Assistant, MessageKind::SubagentSpawn)
        );
        let usage = s.messages[1].usage.as_ref().unwrap();
        assert_eq!(
            (usage.input_tokens, usage.cache_read_tokens, usage.output_tokens),
            (Some(1200), Some(400), Some(95))
        );

        // Step 3: Truncated output path extracted
        assert_eq!(
            s.messages[2].content[0].persisted_output_path(),
            Some("/workspace/demo/.system_generated/steps/3/output.txt")
        );

        // Step 4: Permission denial surfaces via Affordance
        let affs = crate::affordance::of_session(&s).unwrap();
        assert!(
            affs.iter()
                .any(|a| a.kind == crate::affordance::Kind::Approval && a.action == "deny"),
            "expected approval/deny in {affs:?}"
        );
        assert!(
            affs.iter().any(|a| a.kind == crate::affordance::Kind::SubagentSpawn),
            "expected subagent_spawn in {affs:?}"
        );
        assert!(
            affs.iter().any(|a| a.kind == crate::affordance::Kind::SubagentReturn),
            "expected subagent_return in {affs:?}"
        );

        // Step 8: Child SYSTEM_MESSAGE classified as SubagentReturn + delivered_body unwrapped
        assert_eq!(
            (s.messages[5].role, s.messages[5].kind, s.messages[5].origin),
            (Role::User, MessageKind::SubagentReturn, Origin::Subagent)
        );
        let raw_ret = s.messages[5].text().unwrap();
        assert_eq!(
            ROOTLESS.delivered_body(&s.messages[5], &raw_ret),
            Some("Audit complete: 3 exports.")
        );

        // Step 9: CompactionBoundary + CompactionSummary
        assert_eq!(s.messages[6].kind, MessageKind::CompactionBoundary);
        assert_eq!(s.messages[7].kind, MessageKind::CompactionSummary);

        // Step 10: Error message
        assert_eq!(s.messages[8].kind, MessageKind::Error);
    }

    #[test]
    fn subagent_initial_system_message_sets_prompt_and_parent_lineage() {
        let child_jsonl = serde_json::to_string(&json!({
            "step_index": 0,
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "created_at": "2026-11-11T10:00:05Z",
            "content": format!("Created At: 2026-11-11T10:00:05Z\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-11T10:00:05Z sender={PARENT_ID} priority=HIGH content=Audit all exported functions.\n</SYSTEM_MESSAGE>")
        }))
        .unwrap();
        let s = parse_str(CHILD_ID, &child_jsonl, None).unwrap();
        assert_eq!(s.lineage.parent.as_deref(), Some(PARENT_ID));
        assert_eq!(s.messages.len(), 1);
        assert_eq!(
            (s.messages[0].role, s.messages[0].kind, s.messages[0].origin),
            (Role::User, MessageKind::Prompt, Origin::Subagent)
        );
        assert!(s.messages[0].is_turn());
    }

    #[test]
    fn deduplicates_across_roots_and_ignores_chunks_and_encrypted_pb() {
        let world = crate::test_world("antigravity-dedup");
        let gemini = world.base.join(".gemini");
        let cli_logs = gemini
            .join("antigravity-cli/brain")
            .join(PARENT_ID)
            .join(".system_generated/logs");
        let ide_logs = gemini
            .join("antigravity-ide/brain")
            .join(PARENT_ID)
            .join(".system_generated/logs");
        fs::create_dir_all(cli_logs.join("chunks/transcript_full")).unwrap();
        fs::create_dir_all(&ide_logs).unwrap();
        fs::create_dir_all(gemini.join("antigravity-cli/implicit")).unwrap();
        fs::create_dir_all(gemini.join("antigravity-cli/annotations")).unwrap();

        let step = serde_json::to_string(&json!({
            "step_index": 0,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "status": "DONE",
            "created_at": "2026-11-11T10:00:00Z",
            "content": "hello from full log"
        }))
        .unwrap();
        fs::write(cli_logs.join("transcript_full.jsonl"), format!("{step}\n")).unwrap();
        fs::write(cli_logs.join("transcript.jsonl"), format!("{step}\n")).unwrap();
        fs::write(cli_logs.join("chunks/transcript_full/0.jsonl"), format!("{step}\n")).unwrap();
        fs::write(ide_logs.join("overview.txt"), format!("{step}\n")).unwrap();
        fs::write(gemini.join("antigravity-cli/implicit/9999.pb"), [0xffu8; 32]).unwrap();
        fs::write(
            gemini.join("antigravity-cli/history.jsonl"),
            "{\"display\":\"ignored\"}\n",
        )
        .unwrap();
        fs::write(
            gemini
                .join("antigravity-cli/annotations")
                .join(format!("{PARENT_ID}.pbtxt")),
            "title: \"Escaped \\\"Title\\\" Here\"\n",
        )
        .unwrap();

        let adapter = Antigravity::with_home(&world.base);
        let found = adapter.discover().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, PARENT_ID);
        assert_eq!(found[0].path, cli_logs.join("transcript_full.jsonl"));
        assert_eq!(found[0].title.as_deref(), Some("Escaped \"Title\" Here"));

        let parsed = adapter.parse(&found[0]).unwrap();
        assert_eq!(parsed.title.as_deref(), Some("Escaped \"Title\" Here"));
        assert_eq!(parsed.messages.len(), 1);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn enriches_model_cwd_git_and_lineage_from_readonly_sqlite_sidecars() {
        fn pb_varint(mut v: u64, out: &mut Vec<u8>) {
            loop {
                let mut b = (v & 0x7f) as u8;
                v >>= 7;
                if v != 0 {
                    b |= 0x80;
                }
                out.push(b);
                if v == 0 {
                    break;
                }
            }
        }
        fn pb_bytes(field: u64, payload: &[u8], out: &mut Vec<u8>) {
            pb_varint((field << 3) | 2, out);
            pb_varint(payload.len() as u64, out);
            out.extend_from_slice(payload);
        }
        fn pb_u64(field: u64, val: u64, out: &mut Vec<u8>) {
            pb_varint(field << 3, out);
            pb_varint(val, out);
        }

        let world = crate::test_world("antigravity-sqlite");
        let app_dir = world.base.join(".gemini/antigravity-cli");
        let logs_dir = app_dir.join("brain").join(CHILD_ID).join(".system_generated/logs");
        fs::create_dir_all(&logs_dir).unwrap();
        fs::create_dir_all(app_dir.join("conversations")).unwrap();

        let lines = [
            serde_json::to_string(&json!({
                "step_index": 0,
                "source": "USER_EXPLICIT",
                "type": "USER_INPUT",
                "status": "DONE",
                "created_at": "2026-11-11T12:00:00Z",
                "content": "run check"
            }))
            .unwrap(),
            serde_json::to_string(&json!({
                "step_index": 1,
                "source": "MODEL",
                "type": "PLANNER_RESPONSE",
                "status": "DONE",
                "created_at": "2026-11-11T12:00:01Z",
                "content": "ok",
                "input_tokens": 50,
                "cache_read_tokens": 10,
                "output_tokens": 5
            }))
            .unwrap(),
        ]
        .join("\n");
        fs::write(logs_dir.join("transcript_full.jsonl"), format!("{lines}\n")).unwrap();

        // Build `conversation_summaries.db`
        let sum_db = app_dir.join("conversation_summaries.db");
        {
            let conn = rusqlite::Connection::open(&sum_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE conversation_summaries (
                    conversation_id TEXT PRIMARY KEY,
                    title TEXT,
                    preview TEXT,
                    workspace_uris TEXT,
                    parent_conversation_id TEXT,
                    agent_name TEXT,
                    source TEXT,
                    project_id TEXT,
                    last_modified_time TEXT
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO conversation_summaries VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    CHILD_ID,
                    "SQLite Summary Title",
                    "preview text",
                    "[\"file:///workspace/demo\"]",
                    PARENT_ID,
                    "research",
                    "CORTEX_TRAJECTORY_SOURCE_CLI",
                    "proj-demo",
                    "2026-11-11T12:00:05Z"
                ],
            )
            .unwrap();
        }

        // Build `conversations/<uuid>.db` with `gen_metadata` and `trajectory_metadata_blob`
        let conv_db = app_dir.join("conversations").join(format!("{CHILD_ID}.db"));
        {
            let conn = rusqlite::Connection::open(&conv_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);
                 CREATE TABLE trajectory_metadata_blob (id TEXT PRIMARY KEY, data BLOB);",
            )
            .unwrap();
            let mut sub_model = Vec::new();
            pb_bytes(19, b"gemini-4-argon", &mut sub_model);
            let mut packed_steps = Vec::new();
            pb_varint(1, &mut packed_steps);
            let mut gen_blob = Vec::new();
            pb_bytes(1, &sub_model, &mut gen_blob);
            pb_bytes(2, &packed_steps, &mut gen_blob);
            conn.execute("INSERT INTO gen_metadata (idx, data) VALUES (0, ?1)", [&gen_blob])
                .unwrap();

            let mut ws_sub = Vec::new();
            pb_bytes(1, b"file:///workspace/demo", &mut ws_sub);
            pb_bytes(2, b"file:///workspace/demo", &mut ws_sub);
            pb_bytes(4, b"feat/antigravity", &mut ws_sub);
            let mut ts_sub = Vec::new();
            pb_u64(1, 1_794_398_400, &mut ts_sub);
            let mut traj_blob = Vec::new();
            pb_bytes(1, &ws_sub, &mut traj_blob);
            pb_bytes(2, &ts_sub, &mut traj_blob);
            conn.execute(
                "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
                [&traj_blob],
            )
            .unwrap();
        }

        let adapter = Antigravity::with_home(&world.base);
        let refs = adapter.discover().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].cwd.as_deref(), Some(Path::new("/workspace/demo")));
        assert_eq!(refs[0].title.as_deref(), Some("SQLite Summary Title"));

        let s = adapter.parse(&refs[0]).unwrap();
        crate::harness::assert_no_flat_keys(&s);
        assert_eq!(s.model.as_deref(), Some("gemini-4-argon"));
        assert_eq!(s.messages[1].model.as_deref(), Some("gemini-4-argon"));
        assert_eq!(
            s.git.as_ref().and_then(|g| g.branch.as_deref()),
            Some("feat/antigravity")
        );
        assert_eq!(s.lineage.parent.as_deref(), Some(PARENT_ID));
        assert_eq!(s.lineage.agent_path.as_deref(), Some("research"));

        // Ensure zero `-wal` / `-shm` coordination files were created next to either SQLite DB!
        for db in [&sum_db, &conv_db] {
            for suffix in ["-wal", "-shm", "-journal"] {
                let mut p = db.as_os_str().to_os_string();
                p.push(suffix);
                assert!(!Path::new(&p).exists(), "created sidecar {}", Path::new(&p).display());
            }
        }
    }

    #[test]
    fn step_peek_handles_large_preambles_and_structured_conversation_id_extraction() {
        // Quoted prose containing `"conversationId": "spoof"` inside another field must not hijack
        // structured JSON extraction, and payloads past 1 KiB (up to 64 KiB) still resolve!
        let padding = "x".repeat(4096);
        let payload = serde_json::json!({
            "note": format!("prose mentioning {{\"conversationId\":\"spoofed-id\"}} {padding}"),
            "result": {
                "conversationId": "real-child-uuid-9999"
            }
        })
        .to_string();
        assert_eq!(
            extract_spawned_conversation_id(&payload).as_deref(),
            Some("real-child-uuid-9999")
        );

        let parent_id = "11111111-1111-4111-8111-111111111111";
        let child_id = "22222222-2222-4222-8222-222222222222";
        let long_header = " ".repeat(2048);
        let transcript = [
            serde_json::json!({
                "step_index": 0,
                "type": "SYSTEM_MESSAGE",
                "content": format!("{long_header}<SYSTEM_MESSAGE>\n[Message] sender={parent_id}\nAssigned work\n</SYSTEM_MESSAGE>")
            })
            .to_string(),
            serde_json::json!({
                "step_index": 1,
                "type": "TOOL_RESULT",
                "tool_call_id": "tc_sub",
                "tool_name": "invoke_subagent",
                "content": serde_json::json!({
                    "log": padding,
                    "echo": "{\"conversationId\":\"wrong\"}",
                    "spawn": { "conversationId": child_id }
                }).to_string()
            })
            .to_string(),
        ]
        .join("\n");

        let parsed = parse_str("33333333-3333-4333-8333-333333333333", &transcript, None).unwrap();
        assert_eq!(parsed.lineage.parent.as_deref(), Some(parent_id));
        assert_eq!(parsed.messages[0].origin, Origin::Subagent);
    }
}
