use crate::ir::Harness;
use std::path::PathBuf;

#[derive(Clone, Copy)]
enum Base {
    Env(&'static str),
    Home,
}
use Base::{Env as E, Home as H};

#[rustfmt::skip]
fn specs(h: Harness) -> &'static [(Base, &'static str, &'static [&'static str])] {
    match h {
        Harness::Claude => &[
            (E("CLAUDE_CONFIG_DIR"), "projects", &["$CLAUDE_CONFIG_DIR/projects", "${CLAUDE_CONFIG_DIR}/projects"]),
            (E("CLAUDE_CONFIG_DIR"), "transcripts", &[]),
            (H, ".claude/projects", &["/.claude/projects"]), (H, ".claude/transcripts", &["/.claude/transcripts"])],
        Harness::Codex => &[
            (E("CODEX_HOME"), "sessions", &["$CODEX_HOME/sessions", "${CODEX_HOME}/sessions"]),
            (E("CODEX_HOME"), "archived_sessions", &[]),
            (H, ".codex/sessions", &["/.codex/sessions"]), (H, ".codex/archived_sessions", &["/.codex/archived_sessions"])],
        Harness::Grok => &[(H, ".grok", &[])],
        Harness::OpenCode => &[
            (E("XDG_DATA_HOME"), "opencode", &[]), (H, ".local/share/opencode", &["/.local/share/opencode"])],
        Harness::Gemini => &[(H, ".gemini/tmp", &["/.gemini/tmp"])],
        Harness::Antigravity => &[
            (E("ANTIGRAVITY_APP_DATA_DIR"), "brain", &[]), (E("ANTIGRAVITY_APP_DATA_DIR"), "conversations", &[]),
            (E("JETSKI_APP_DATA_DIR"), "brain", &[]), (E("JETSKI_APP_DATA_DIR"), "conversations", &[]),
            (H, ".gemini/antigravity", &["/.gemini/antigravity"]), (H, ".gemini/antigravity-cli", &[])],
        Harness::Hermes => &[(E("HERMES_HOME"), "", &[]), (H, ".hermes", &["/.hermes"])],
        Harness::OpenClaw => &[
            (E("OPENCLAW_STATE_DIR"), "agents", &[]), (H, ".openclaw/agents", &["/.openclaw/agents"])],
        Harness::Pi => &[(H, ".pi/agent/sessions", &["/.pi/agent/sessions"])],
        Harness::Cursor => &[
            (H, "Library/Application Support/Cursor/User/globalStorage", &[]),
            (H, "Library/Application Support/Cursor/User/workspaceStorage", &[]),
            (H, ".cursor/chats", &[]), (H, ".cursor/acp-sessions", &[])],
        Harness::Kimi => &[
            (E("XDG_DATA_HOME"), "kimi", &[]), (H, ".local/share/kimi", &[]),
            (H, ".kimi/sessions", &["/.kimi/sessions"])],
        Harness::Qwen => &[(H, ".qwen/projects", &["/.qwen/projects"]), (H, ".qwen/tmp", &[])],
        Harness::KimiCode | Harness::LmStudio | Harness::Cline | Harness::Roo => &[],
        Harness::Continue => &[(H, ".continue/sessions", &["/.continue/sessions"])],
        Harness::Goose => &[
            (E("XDG_DATA_HOME"), "goose/sessions", &[]),
            (H, ".local/share/goose/sessions", &["/.local/share/goose/sessions"]),
            (H, "Library/Application Support/Block.block.goose/sessions", &[])],
        Harness::Zed => &[
            (E("XDG_DATA_HOME"), "zed/threads", &[]),
            (H, ".local/share/zed/threads", &["/.local/share/zed/threads"]),
            (H, "Library/Application Support/Zed/threads", &[])],
        Harness::Devin => &[
            (E("XDG_DATA_HOME"), "devin/cli", &[]), (H, ".local/share/devin/cli", &["/.local/share/devin/cli"])],
        Harness::ChatGptExport | Harness::ClaudeExport | Harness::OpenSession => &[],
    }
}
fn base_dir(base: Base) -> Option<PathBuf> {
    match base {
        E(var) => crate::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from),
        H => crate::env::var_os("HOME").map(PathBuf::from),
    }
}

/// Known harness transcript store roots from the active environment (`HOME`, `CLAUDE_CONFIG_DIR`,
/// `CODEX_HOME`, `XDG_DATA_HOME`, etc.), constructed in memory with zero disk discovery.
pub fn transcript_store_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for &(base, path, _) in Harness::ALL.into_iter().flat_map(specs) {
        let Some(base) = base_dir(base) else { continue };
        let path = if path.is_empty() { base } else { base.join(path) };
        roots.push(crate::tools::classify::normalize_path(&path, None));
        roots.push(path);
    }
    roots.sort();
    roots.dedup();
    roots
}

/// Shell spellings of transcript store roots the nudge recognizes, in registry order.
pub fn shell_store_markers() -> impl Iterator<Item = &'static str> {
    let specs = Harness::ALL.into_iter().flat_map(specs);
    specs.flat_map(|(_, _, markers)| markers.iter().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn shell_markers_are_the_recognized_store_spellings() {
        let markers: BTreeSet<_> = shell_store_markers().collect();
        let expected: BTreeSet<_> = [
            "/.claude/projects",
            "/.claude/transcripts",
            "$CLAUDE_CONFIG_DIR/projects",
            "${CLAUDE_CONFIG_DIR}/projects",
            "/.codex/sessions",
            "/.codex/archived_sessions",
            "$CODEX_HOME/sessions",
            "${CODEX_HOME}/sessions",
            "/.gemini/tmp",
            "/.gemini/antigravity",
            "/.local/share/opencode",
            "/.local/share/devin/cli",
            "/.local/share/zed/threads",
            "/.local/share/goose/sessions",
            "/.hermes",
            "/.openclaw/agents",
            "/.kimi/sessions",
            "/.qwen/projects",
            "/.continue/sessions",
            "/.pi/agent/sessions",
        ]
        .into_iter()
        .collect();
        assert_eq!(markers, expected);
    }

    #[test]
    fn store_roots_read_the_harness_env_vars_and_home() {
        let home = "/cv-store-roots-test/home";
        let _env = crate::env::Env::from_vars([
            ("HOME", home),
            ("CLAUDE_CONFIG_DIR", "/cv-store-roots-test/claude"),
            ("CODEX_HOME", "/cv-store-roots-test/codex"),
            ("ANTIGRAVITY_APP_DATA_DIR", "/cv-store-roots-test/agy"),
            ("JETSKI_APP_DATA_DIR", "/cv-store-roots-test/jetski"),
            ("HERMES_HOME", "/cv-store-roots-test/hermes"),
            ("OPENCLAW_STATE_DIR", "/cv-store-roots-test/openclaw"),
            ("XDG_DATA_HOME", "/cv-store-roots-test/xdg"),
        ])
        .enter();
        let roots: BTreeSet<_> = transcript_store_roots().into_iter().collect();
        for expected in [
            "/cv-store-roots-test/claude/projects",
            "/cv-store-roots-test/claude/transcripts",
            "/cv-store-roots-test/codex/sessions",
            "/cv-store-roots-test/codex/archived_sessions",
            "/cv-store-roots-test/agy/brain",
            "/cv-store-roots-test/agy/conversations",
            "/cv-store-roots-test/jetski/brain",
            "/cv-store-roots-test/jetski/conversations",
            "/cv-store-roots-test/hermes",
            "/cv-store-roots-test/openclaw/agents",
            "/cv-store-roots-test/xdg/opencode",
            "/cv-store-roots-test/xdg/zed/threads",
            "/cv-store-roots-test/xdg/goose/sessions",
            "/cv-store-roots-test/xdg/devin/cli",
            "/cv-store-roots-test/xdg/kimi",
        ] {
            assert!(roots.contains(&PathBuf::from(expected)), "missing {expected}");
        }
        for expected in [
            ".claude/projects",
            ".claude/transcripts",
            ".codex/sessions",
            ".codex/archived_sessions",
            ".gemini/tmp",
            ".gemini/antigravity",
            ".gemini/antigravity-cli",
            ".local/share/opencode",
            ".local/share/zed/threads",
            ".local/share/goose/sessions",
            ".local/share/devin/cli",
            ".local/share/kimi",
            "Library/Application Support/Cursor/User/globalStorage",
            "Library/Application Support/Cursor/User/workspaceStorage",
            "Library/Application Support/Zed/threads",
            "Library/Application Support/Block.block.goose/sessions",
            ".cursor/chats",
            ".cursor/acp-sessions",
            ".hermes",
            ".openclaw/agents",
            ".kimi/sessions",
            ".qwen/projects",
            ".qwen/tmp",
            ".continue/sessions",
            ".pi/agent/sessions",
            ".grok",
        ] {
            assert!(
                roots.contains(&PathBuf::from(home).join(expected)),
                "missing home {expected}"
            );
        }
    }
}
