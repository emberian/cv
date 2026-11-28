//! CLI report guard over [`cv_core::custody::ScopedView`].
use anyhow::{Context, Result};
use cv_core::custody::{ScopedSource, ScopedView};
use cv_core::state::hub::HubStore;
use cv_core::{Adapter, Harness, SessionRef};
use std::{cell::RefCell, rc::Rc};

pub(super) struct View {
    pub inner: ScopedView,
    pub args: crate::AuthorityArgs,
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
                let a = v.inner.envelope.borrow();
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
    let view = Rc::new(View {
        inner: ScopedView::new(hub, authority, args.as_of.is_some()),
        args: args.clone(),
    });
    Ok(Guard(CURRENT.with(|c| c.replace(Some(view))), human))
}

fn map_resolve(spec: &str, kind: &str, res: Result<ScopedSource>) -> Result<ScopedSource> {
    match res {
        Ok(src) => Ok(src),
        Err(e) => match cv_core::ResolveError::of(&e) {
            Some(cv_core::ResolveError::NotFound(_)) => {
                crate::util::usage(format!("no {kind} {spec:?} in the selected places"))
            }
            Some(cv_core::ResolveError::Ambiguous(_)) => {
                crate::util::usage(format!("ambiguous {kind} {spec:?} in the selected places"))
            }
            None => Err(e),
        },
    }
}

impl View {
    pub fn pinned(&self) -> bool {
        self.inner.pinned
    }

    pub fn sources(&self) -> Result<&[ScopedSource]> {
        self.inner.sources()
    }

    pub fn resolve(&self, spec: &str, harness: Option<Harness>) -> Result<ScopedSource> {
        map_resolve(spec, "session", self.inner.resolve(spec, harness))
    }

    pub fn source_for(&self, r: &SessionRef) -> Result<ScopedSource> {
        self.inner.source_for(r)
    }

    pub fn adapter(&self, src: &ScopedSource) -> Result<Box<dyn Adapter>> {
        self.inner.adapter(src)
    }

    pub fn children(&self, parent: &ScopedSource) -> Result<Vec<ScopedSource>> {
        self.inner.children(parent)
    }

    pub fn child(&self, parent: &ScopedSource, id: &str) -> Result<ScopedSource> {
        map_resolve(id, "agent", self.inner.child(parent, id))
    }

    pub fn agent_id(src: &ScopedSource) -> &str {
        ScopedView::agent_id(src)
    }

    pub fn omit(&self, place: &str, reason: &str, detail: &str) {
        self.inner.omit(place, reason, detail);
    }
}

pub(super) fn print_json(key: &str, value: &impl serde::Serialize) -> Result<()> {
    if let Some(view) = current() {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({(key):value,"authority":*view.inner.envelope.borrow()}))?
        );
    } else {
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}
