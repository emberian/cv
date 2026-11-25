//! Logical session identity shared by discovery, search and custody.
use crate::{Harness, SessionRef};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub producer: String,
    pub profile: PathBuf,
    pub harness: Harness,
    pub native_id: String,
}

fn is_profile_sessions_dir(harness: Harness, p: &Path) -> bool {
    match harness {
        Harness::Claude => p.ends_with("projects"),
        Harness::Codex => p.ends_with("sessions") || p.ends_with("archived_sessions"),
        Harness::Gemini
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Qwen
        | Harness::Grok
        | Harness::Hermes
        | Harness::OpenClaw
        | Harness::Cursor
        | Harness::Antigravity
        | Harness::LmStudio
        | Harness::OpenCode
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::Pi
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => false,
    }
}

fn uses_subagents_dir(harness: Harness) -> bool {
    match harness {
        Harness::Claude => true,
        Harness::Codex
        | Harness::Gemini
        | Harness::Kimi
        | Harness::KimiCode
        | Harness::Qwen
        | Harness::Grok
        | Harness::Hermes
        | Harness::OpenClaw
        | Harness::Cursor
        | Harness::Antigravity
        | Harness::LmStudio
        | Harness::OpenCode
        | Harness::Cline
        | Harness::Roo
        | Harness::Continue
        | Harness::Goose
        | Harness::Zed
        | Harness::Devin
        | Harness::Pi
        | Harness::ChatGptExport
        | Harness::ClaudeExport
        | Harness::OpenSession => false,
    }
}

impl SessionKey {
    pub fn local(r: &SessionRef) -> Self {
        let root = r.path.ancestors().find(|p| is_profile_sessions_dir(r.harness, p));
        Self {
            producer: "local".into(),
            profile: root
                .and_then(Path::parent)
                .or_else(|| {
                    r.path
                        .ancestors()
                        .find(|p| uses_subagents_dir(r.harness) && p.ends_with("subagents"))
                        .and_then(|p| p.parent()?.parent())
                })
                .or_else(|| r.path.parent())
                .unwrap_or(&r.path)
                .to_owned(),
            harness: r.harness,
            native_id: r.id.clone(),
        }
    }

    /// A CLI selector and exact index term. Length framing keeps arbitrary component bytes distinct.
    pub fn selector(&self) -> String {
        let mut hash = blake3::Hasher::new();
        for part in [
            self.producer.as_bytes(),
            self.profile.as_os_str().as_encoded_bytes(),
            self.harness.as_str().as_bytes(),
        ] {
            hash.update(&(part.len() as u64).to_le_bytes());
            hash.update(part);
        }
        format!("{}@{}", self.native_id, hash.finalize().to_hex())
    }

    pub fn split_selector(s: &str) -> Option<(&str, &str)> {
        let (id, suffix) = s.rsplit_once('@')?;
        (suffix.len() == 64 && suffix.bytes().all(|b| b.is_ascii_hexdigit())).then_some((id, suffix))
    }
}

impl SessionRef {
    pub fn session_selector(&self) -> String {
        if self.path.to_string_lossy().ends_with(".pack.zst") && SessionKey::split_selector(&self.id).is_some() {
            return self.id.clone();
        }
        SessionKey::local(self).selector()
    }
}
