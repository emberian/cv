//! Session selection for reports that read a mixture of local transcripts and published revisions.
use anyhow::{Context, Result};
use cv_core::state::hub::{AuthorityEnvelope, AuthorityOmission, AuthorityStatus, HubStore, SessionRevisionEntry};
use cv_core::{Adapter, Harness, SessionRef};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone)]
pub(super) struct Source {
    pub r: SessionRef,
    pub entry: Option<SessionRevisionEntry>,
}

pub(super) struct View {
    pub sources: Vec<Source>,
    pub envelope: RefCell<AuthorityEnvelope>,
    pub pinned: bool,
    pub args: crate::AuthorityArgs,
    hub: HubStore,
}

thread_local! {static CURRENT: RefCell<Option<Rc<View>>> = const {RefCell::new(None)};}
pub(super) fn current() -> Option<Rc<View>> {
    CURRENT.with(|c| c.borrow().clone())
}

pub(crate) struct Guard(Option<Rc<View>>, bool);
impl Drop for Guard {
    fn drop(&mut self) {
        if self.1 {
            if let Some(v) = current() {
                let a = v.envelope.borrow();
                eprintln!(
                    "cv: {} at generation {} (indexed {})",
                    a.status.as_str(),
                    a.generation,
                    a.indexed_generation
                );
                for o in &a.omissions {
                    eprintln!(
                        "  {}: {}",
                        cv_core::sanitize::sanitize_line(&o.place_id),
                        cv_core::sanitize::sanitize_line(&o.detail)
                    );
                }
            }
        }
        CURRENT.with(|c| c.replace(self.0.take()));
    }
}

pub(crate) fn enter(args: &crate::AuthorityArgs, cwd: Option<&str>, query: Option<&str>, human: bool) -> Result<Guard> {
    let authority =
        super::browse::evaluate_cli_authority(args, cwd, query, false, false)?.context("missing place scope")?;
    let hub = HubStore::default_hub()?;
    let mut sources = vec![];
    if args.as_of.is_none() && authority.includes("local") {
        for r in cv_core::try_sessions_for(None)? {
            if !r.path.starts_with(hub.packs_dir()) {
                sources.push(Source { r, entry: None });
            }
        }
    }
    if authority.generation != 0 {
        for entry in hub.heads_at_generation(Some(authority.generation))? {
            if authority.includes(&entry.producer_id) {
                if let Some(r) = cv_core::api::hub_head_to_ref(hub.packs_dir(), &entry) {
                    sources.push(Source { r, entry: Some(entry) });
                }
            }
        }
    }
    let view = Rc::new(View {
        sources,
        envelope: RefCell::new(authority),
        pinned: args.as_of.is_some(),
        args: args.clone(),
        hub,
    });
    Ok(Guard(CURRENT.with(|c| c.replace(Some(view))), human))
}

impl View {
    pub fn resolve(&self, spec: &str, harness: Option<Harness>) -> Result<&Source> {
        let (want, id) = crate::util::split_harness_id(spec, harness);
        let candidates: Vec<_> = self
            .sources
            .iter()
            .filter(|s| want.is_none_or(|h| s.r.harness == h))
            .collect();
        let native = |s: &Source| {
            s.entry
                .as_ref()
                .and_then(|e| e.session_key_struct())
                .map(|k| k.native_id)
                .unwrap_or_else(|| s.r.id.clone())
        };
        let exact: Vec<_> = candidates
            .iter()
            .copied()
            .filter(|s| {
                s.r.id == id || s.r.session_selector() == id || native(s) == id || s.r.path == std::path::Path::new(id)
            })
            .collect();
        let hits = if exact.is_empty() {
            candidates
                .into_iter()
                .filter(|s| s.r.id.starts_with(id) || native(s).starts_with(id))
                .collect()
        } else {
            exact
        };
        match hits.as_slice() {
            [s] => Ok(*s),
            [] => crate::util::usage(format!("no session {spec:?} in the selected places")),
            _ => crate::util::usage(format!("ambiguous session {spec:?} in the selected places")),
        }
    }

    pub fn source_for(&self, r: &SessionRef) -> Result<&Source> {
        self.sources
            .iter()
            .find(|s| s.r.id == r.id && s.r.harness == r.harness && s.r.path == r.path)
            .context("session is outside the selected places")
    }
    pub fn adapter(&self, src: &Source) -> Result<Box<dyn Adapter>> {
        if let Some(entry) = &src.entry {
            return Ok(Box::new(
                cv_core::custody::backing_from_hub_head(self.hub.packs_dir(), entry.clone())?
                    .context("revision payload was not collected")?,
            ));
        }
        cv_core::custody::backing_for(&src.r)?
            .or_else(|| cv_core::harness::for_harness(src.r.harness))
            .context("session reader unavailable")
    }

    pub fn children<'a>(&'a self, parent: &'a Source) -> impl Iterator<Item = &'a Source> {
        self.sources.iter().filter(move |s| {
            let (Some(p), Some(child)) = (&parent.entry, &s.entry) else {
                return false;
            };
            let native = p.metadata["native_id"].as_str().unwrap_or(&p.session_id);
            child.producer_id == p.producer_id
                && child.metadata["profile"] == p.metadata["profile"]
                && child.metadata["parent_id"]
                    .as_str()
                    .is_some_and(|id| id == native || id == parent.r.id)
        })
    }

    pub fn child<'a>(&'a self, parent: &'a Source, id: &str) -> Result<&'a Source> {
        let mut hits = self.children(parent).filter(|s| Self::agent_id(s).starts_with(id));
        let first = hits
            .next()
            .context("child transcript not collected in selected revision")?;
        anyhow::ensure!(hits.next().is_none(), "ambiguous agent {id:?}");
        Ok(first)
    }

    pub fn agent_id(src: &Source) -> &str {
        src.entry
            .as_ref()
            .and_then(|e| {
                e.metadata["agent_id"]
                    .as_str()
                    .or_else(|| e.metadata["native_id"].as_str())
                    .or(Some(&e.session_id))
            })
            .unwrap_or(&src.r.id)
            .trim_start_matches("agent-")
    }
    pub fn events(&self, src: &Source) -> Result<Vec<cv_core::events::Event>> {
        let mut sink = cv_core::events::EventSink::new(src.r.cwd.clone()).with_source(src.r.path.clone());
        self.adapter(src)?
            .try_stream(&src.r, &cv_core::Demand::LEAN.with_spans(), &mut sink)?;
        sink.into_events()
    }

    pub fn event_rows(&self, src: &Source) -> Result<Vec<cv_core::events::EventRow>> {
        Ok(self
            .events(src)?
            .into_iter()
            .map(|e| cv_core::events::EventRow {
                baggage: None,
                harness: src.r.harness.to_string(),
                session_id: src.r.id.clone(),
                msg_idx: e.msg_idx as i64,
                ts: e.ts,
                kind: e.kind.into(),
                tool: e.tool,
                target: e.target,
                detail: e.detail,
                agent_id: None,
                parent_id: None,
                workflow: None,
            })
            .collect())
    }
    pub fn omit(&self, place: &str, reason: &str, detail: &str) {
        let mut env = self.envelope.borrow_mut();
        let omission = AuthorityOmission {
            place_id: place.into(),
            reason: reason.into(),
            detail: detail.into(),
        };
        if !env.omissions.contains(&omission) {
            env.omissions.push(omission);
        }
        env.status = AuthorityStatus::Partial;
    }
}

pub(super) fn print_json(key: &str, value: &impl serde::Serialize) -> Result<()> {
    if let Some(view) = current() {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({(key):value,"authority":*view.envelope.borrow()}))?
        );
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}
