//! A distilled pack as a new Antigravity conversation (`cv distill --session=antigravity`): one
//! user prompt, written cold into an app-data root ([`super::emit`]) or delivered live to a running
//! language server ([`super::deliver`]).

use super::deliver::{deliver_live_seed, Delivery, Seed};
use super::{Adapter as _, Antigravity};
use crate::ir::{Block, Harness, Message, MessageKind, Origin, Role, Session};
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;

const LS_ADDRESS: &str = "ANTIGRAVITY_LS_ADDRESS";
const CSRF_TOKEN: &str = "ANTIGRAVITY_CSRF_TOKEN";

/// Whether the environment names a live language server, so delivery defaults to live.
pub fn live_configured() -> bool {
    crate::env::var_os(LS_ADDRESS).is_some_and(|v| !v.is_empty())
}

fn project_id(source: &Session) -> &str {
    source
        .harness_extra(Harness::Antigravity)
        .and_then(|v| v.get("project_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// The one-prompt session [`super::emit`] writes cold: `pack` as a human prompt, titled `title`,
/// carrying `source`'s timestamps and Antigravity metadata.
pub fn cold_session(source: &Session, pack: &str, title: &str, cwd: Option<PathBuf>) -> Session {
    let mut seed = Session::new(crate::prune::new_uuid(), Harness::Antigravity);
    seed.cwd = cwd;
    seed.title = Some(title.to_owned());
    seed.created_at = source.created_at;
    seed.updated_at = source.updated_at;
    if let Some(extra) = source.harness_extra(Harness::Antigravity).cloned() {
        seed.extra
            .insert(Harness::Antigravity.as_str().to_owned(), Value::Object(extra));
    }
    let mut prompt = Message::of_kind(Role::User, MessageKind::Prompt, Origin::Human);
    prompt.timestamp = source.updated_at.or(source.created_at);
    prompt.content.push(Block::from_text(pack.to_owned().into()));
    seed.messages.push(prompt);
    seed
}

/// The detected Antigravity app-data root a cold seed is written into when `--out` is absent.
pub fn default_root() -> Result<PathBuf> {
    Antigravity::detect()
        .roots()
        .into_iter()
        .next()
        .context("Antigravity store was not found; pass --out <dir>")
}

/// The live request for `pack`: tagged by `source`, its compaction boundary, and its latest model.
pub fn live_seed(source: &Session, boundary_ordinal: Option<usize>, title: &str, pack: &str) -> Seed {
    let model = source
        .messages
        .iter()
        .rev()
        .filter_map(|m| m.model.as_deref())
        .find(|m| !m.trim().is_empty())
        .or_else(|| source.model.as_deref().filter(|m| !m.trim().is_empty()))
        .unwrap_or("");
    Seed::new(&source.id, boundary_ordinal, model, title, project_id(source), pack)
}

/// Deliver `seed` to the language server the environment names, journaled in the default state
/// store.
pub fn deliver_from_env(seed: &Seed, force_retry: bool) -> Result<Delivery> {
    let ls_addr = crate::env::var(LS_ADDRESS)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .with_context(|| {
            format!(
                "{LS_ADDRESS} is not set; pass --out <dir> for an offline root or set {LS_ADDRESS} and {CSRF_TOKEN}"
            )
        })?;
    let csrf = crate::env::var(CSRF_TOKEN)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .with_context(|| format!("{CSRF_TOKEN} is not set"))?;
    let store = crate::state::StateStore::default_store()?;
    deliver_live_seed(&store, &ls_addr, &csrf, seed, force_retry)
}
