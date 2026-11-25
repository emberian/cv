//! The OpenClaw emitter: an IR [`Session`] written into OpenClaw's own on-disk format — the codec run
//! backwards plus a store writer. Registered in `crate::emit::emitter_for`; the shared skeleton
//! (`EmitOptions`, `effective_cwd`, the fidelity table) is in [`crate::emit`].

use crate::emit::*;
use crate::harness::store::JsonlStage;
use crate::harness::wire::anthropic;
use crate::harness::EmitResult;
use crate::ir::{Block, Harness, MessageKind, Role, Session};
use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::Path;
use uuid::Uuid;

/// Emit into OpenClaw's `agents/<agentId>/sessions/<sid>.jsonl` transcript (v3 parent-linked) plus
/// a `sessions.json` index entry. Mirrors `harness/openclaw/record.rs`: a `{type:session,…}`
/// header line followed by `{type:message,id,parentId,timestamp,message:{role,content,…}}` lines.
pub fn emit(session: &Session, out_dir: &Path, opts: &EmitOptions) -> Result<EmitResult> {
    stream::emit(session, out_dir, opts, None)
}

pub(crate) fn emit_staged(
    session: &Session,
    out_dir: &Path,
    opts: &EmitOptions,
    verify: Option<crate::emit::stage::VerifyFn<'_>>,
) -> Result<EmitResult> {
    stream::emit(session, out_dir, opts, verify)
}

mod replay;
mod stream;
#[cfg(test)]
mod tests;

/// IR [`Usage`](crate::ir::Usage) → OpenClaw's `usage{input, output, cacheRead, cacheWrite,
/// cost{total}}`, reversing [`openclaw::parse_usage`]'s names. Only the fields present are written;
/// `None` when the turn carried no counts and no cost (nothing its reader would accept).
/// `totalTokens` is never written: the IR holds no recorded total, and OpenClaw's own reader
/// (`normalizeUsage`, `computeUsageTokenTotals`) takes an absent total as input + output +
/// cacheRead + cacheWrite.
fn openclaw_usage(usage: Option<&crate::ir::Usage>) -> Option<Value> {
    let u = usage?;
    let mut m = Map::new();
    for (key, val) in [
        ("input", u.input_tokens),
        ("output", u.output_tokens),
        ("cacheRead", u.cache_read_tokens),
        ("cacheWrite", u.cache_creation_tokens),
    ] {
        if let Some(v) = val {
            m.insert(key.into(), json!(v));
        }
    }
    if let Some(c) = u.cost_usd {
        m.insert("cost".into(), json!({ "total": c }));
    }
    (!m.is_empty()).then_some(Value::Object(m))
}

/// OpenClaw's eight-hex entry IDs, stable for a target session and a record identity.
fn openclaw_short_id(namespace: &Uuid, key: &str) -> String {
    Uuid::new_v5(namespace, key.as_bytes()).simple().to_string()[..8].to_string()
}
