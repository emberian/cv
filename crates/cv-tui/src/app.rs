//! Application state + input handling for cv-tui.
//!
//! `App` is a plain state struct; [`App::on_key`] is the single dispatch point that mutates it in
//! response to a key. Rendering ([`crate::ui::draw`]) only *reads* `App` (plus a tiny bit of cached
//! layout it writes back, like the visible transcript height for paging). Keeping all the logic
//! here makes the event loop trivial and the UI a pure projection of state.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use cv_core::board::{self, BoardMessage};
use cv_core::harness::{self, Adapter};
use cv_core::ir::{Harness, Session, SessionRef};

/// Which top-level view is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// The session list (with optional transcript preview when one is selected).
    List,
    /// A focused, full-width transcript reader.
    Transcript,
    /// The coordination board (fleet channel + active claims).
    Board,
}

/// Transient input mode layered on top of the current view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Normal navigation.
    Normal,
    /// Typing into the `/` substring/fuzzy filter line (filters the visible list).
    Filter,
    /// Typing a full-text/semantic search query (`s`).
    Search,
    /// The `?` help overlay is up.
    Help,
}

/// A row in the list: either a discovered session ref or a search hit (which we resolve to a ref).
#[derive(Debug, Clone)]
pub struct Row {
    pub r#ref: SessionRef,
    /// For search results: a one-line snippet/score to show under the row.
    pub snippet: Option<String>,
    pub score: Option<f32>,
}

/// A line of the rendered transcript, pre-classified so the UI can color it without re-parsing.
#[derive(Debug, Clone)]
pub struct TLine {
    pub text: String,
    pub kind: LineKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    RoleUser,
    RoleAssistant,
    RoleSystem,
    RoleTool,
    Text,
    Thinking,
    ToolUse,
    ToolResult,
    ToolError,
    Meta,
}

pub struct App {
    pub should_quit: bool,
    pub view: View,
    pub mode: Mode,

    /// Every discovered session, newest-first (the unfiltered master list).
    pub all_rows: Vec<Row>,
    /// Indices into `all_rows` that pass the current filter, in display order.
    pub filtered: Vec<usize>,
    /// Cursor within `filtered`.
    pub selected: usize,
    /// How many list rows are visible (set by the renderer) — used for PgUp/PgDn + scroll.
    pub list_height: usize,
    /// First visible filtered-index (scroll offset for the list).
    pub list_offset: usize,

    /// The `/` filter text.
    pub filter: String,
    /// The `s` search query buffer (while typing) and whether the current list is search results.
    pub search_input: String,
    pub showing_search: bool,

    /// The currently-opened transcript, if any (parsed lazily on Enter).
    pub open: Option<OpenSession>,
    /// Transcript scroll offset (top visible line index).
    pub transcript_scroll: usize,
    /// Visible transcript height (set by renderer) for paging math.
    pub transcript_height: usize,

    /// Board state.
    pub board_msgs: Vec<BoardMessage>,
    pub board_claims: Vec<(String, String, chrono::DateTime<chrono::Utc>)>,
    pub board_channel: String,

    /// A transient status/error line shown in the footer.
    pub status: Option<String>,
}

/// A parsed, opened session plus its pre-rendered transcript lines.
pub struct OpenSession {
    pub session: Session,
    pub lines: Vec<TLine>,
}

impl App {
    pub fn new() -> App {
        App {
            should_quit: false,
            view: View::List,
            mode: Mode::Normal,
            all_rows: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            list_height: 20,
            list_offset: 0,
            filter: String::new(),
            search_input: String::new(),
            showing_search: false,
            open: None,
            transcript_scroll: 0,
            transcript_height: 20,
            board_msgs: Vec::new(),
            board_claims: Vec::new(),
            board_channel: "fleet".to_string(),
            status: None,
        }
    }

    // ───────────────────────────── data loading ─────────────────────────────

    /// (Re)load all sessions, newest-first, and reset the list to the unfiltered set.
    ///
    /// Uses [`cv_core::sessions`] — the catalog-backed fast read (~ms when warm) — rather than
    /// `discover_all`'s full stat-the-fleet scan (multiple seconds), so startup and `r` are
    /// instant. The catalog probe transparently escalates to a re-scan when anything changed.
    pub fn refresh_sessions(&mut self) {
        let mut refs = cv_core::sessions();
        // Newest-first by updated_at (fall back to created_at).
        refs.sort_by(|a, b| {
            let ka = a.updated_at.or(a.created_at);
            let kb = b.updated_at.or(b.created_at);
            kb.cmp(&ka)
        });
        self.all_rows = refs
            .into_iter()
            .map(|r| Row {
                r#ref: r,
                snippet: None,
                score: None,
            })
            .collect();
        self.showing_search = false;
        self.search_input.clear();
        self.recompute_filter();
        self.status = Some(format!("{} sessions", self.all_rows.len()));
    }

    /// Recompute `filtered` from `all_rows` + `filter`, clamping the selection.
    pub fn recompute_filter(&mut self) {
        let q = self.filter.to_ascii_lowercase();
        self.filtered = self
            .all_rows
            .iter()
            .enumerate()
            .filter(|(_, row)| q.is_empty() || row_matches(row, &q))
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
        self.list_offset = 0;
    }

    /// The currently-selected row, if any.
    pub fn current_row(&self) -> Option<&Row> {
        self.filtered.get(self.selected).and_then(|&i| self.all_rows.get(i))
    }

    // ───────────────────────────── opening / parsing ─────────────────────────────

    /// Parse the selected session into the IR + pre-rendered lines, and switch to the reader.
    pub fn open_selected(&mut self) {
        let Some(row) = self.current_row().cloned() else {
            return;
        };
        self.status = Some(format!("loading {}…", short_id(&row.r#ref.id)));
        match load_session(&row.r#ref) {
            Ok(session) => {
                let lines = render_lines(&session);
                self.open = Some(OpenSession { session, lines });
                self.transcript_scroll = 0;
                self.view = View::Transcript;
                self.status = None;
            }
            Err(e) => {
                self.status = Some(format!("failed to open: {e:#}"));
            }
        }
    }

    // ───────────────────────────── search ─────────────────────────────

    /// Run text search (with a semantic fallback merged in) and replace the list with hits.
    fn run_search(&mut self) {
        let q = self.search_input.trim().to_string();
        if q.is_empty() {
            return;
        }
        self.status = Some(format!("searching “{q}”…"));

        let mut rows: Vec<Row> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Full-text first (always available).
        match cv_search::text_search(None, &q, 50) {
            Ok(hits) => {
                for h in hits {
                    if let Some(row) = hit_to_row(&h, self) {
                        if seen.insert(row.r#ref.id.clone()) {
                            rows.push(row);
                        }
                    }
                }
            }
            Err(e) => {
                self.status = Some(format!("text search failed: {e:#}"));
            }
        }

        // Try semantic and merge any *new* hits after the FTS ones (best-effort).
        if let Some(extra) = try_semantic_search(&q) {
            for h in extra {
                if let Some(row) = hit_to_row(&h, self) {
                    if seen.insert(row.r#ref.id.clone()) {
                        rows.push(row);
                    }
                }
            }
        }

        let n = rows.len();
        if rows.is_empty() {
            // Keep the old list but tell the user nothing matched.
            self.status = Some(format!("no results for “{q}”"));
            return;
        }
        self.all_rows = rows;
        self.showing_search = true;
        self.filter.clear();
        self.selected = 0;
        self.recompute_filter();
        self.view = View::List;
        self.status = Some(format!("{n} results for “{q}” (Enter opens, r resets)"));
    }

    // ───────────────────────────── board ─────────────────────────────

    fn refresh_board(&mut self) {
        match board::read(&self.board_channel, None, 50) {
            Ok(m) => self.board_msgs = m,
            Err(e) => self.status = Some(format!("board read failed: {e:#}")),
        }
        self.board_claims = board::active_claims(&self.board_channel).unwrap_or_default();
    }

    // ───────────────────────────── navigation helpers ─────────────────────────────

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let max = self.filtered.len() as isize - 1;
        let next = (self.selected as isize + delta).clamp(0, max);
        self.selected = next as usize;
        // Keep the selection within the visible window.
        if self.selected < self.list_offset {
            self.list_offset = self.selected;
        } else if self.selected >= self.list_offset + self.list_height {
            self.list_offset = self.selected + 1 - self.list_height;
        }
    }

    fn scroll_transcript(&mut self, delta: isize) {
        let max = self.open.as_ref().map(|o| o.lines.len().saturating_sub(1)).unwrap_or(0);
        let next = (self.transcript_scroll as isize + delta).clamp(0, max as isize);
        self.transcript_scroll = next as usize;
    }

    // ───────────────────────────── input dispatch ─────────────────────────────

    pub fn on_key(&mut self, key: KeyEvent) {
        // Ignore key-release events (Windows/Kitty send them); we only act on press/repeat.
        if key.kind == KeyEventKind::Release {
            return;
        }

        // Ctrl-C quits from *anywhere* — including while typing into the filter/search line,
        // where it must not be swallowed as a literal 'c'.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }

        // Help overlay swallows everything until dismissed.
        if self.mode == Mode::Help {
            self.mode = Mode::Normal;
            return;
        }

        match self.mode {
            Mode::Filter => self.on_key_filter(key),
            Mode::Search => self.on_key_search(key),
            Mode::Normal => self.on_key_normal(key),
            Mode::Help => {}
        }
    }

    fn on_key_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
                self.recompute_filter();
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                if self.current_row().is_some() {
                    self.open_selected();
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.recompute_filter();
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Char(c) if is_text_input(key) => {
                self.filter.push(c);
                self.recompute_filter();
            }
            _ => {}
        }
    }

    fn on_key_search(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.search_input.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.run_search();
            }
            KeyCode::Backspace => {
                self.search_input.pop();
            }
            KeyCode::Char(c) if is_text_input(key) => self.search_input.push(c),
            _ => {}
        }
    }

    fn on_key_normal(&mut self, key: KeyEvent) {
        // Global keys that work in any view.
        match key.code {
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                return;
            }
            KeyCode::Tab => {
                self.cycle_view();
                return;
            }
            _ => {}
        }

        match self.view {
            View::List => self.on_key_list(key),
            View::Transcript => self.on_key_transcript(key),
            View::Board => self.on_key_board(key),
        }
    }

    fn on_key_list(&mut self, key: KeyEvent) {
        let page = self.list_height.max(1) as isize;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('r') => self.refresh_sessions(),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('s') => {
                self.search_input.clear();
                self.mode = Mode::Search;
            }
            KeyCode::Char('b') => {
                self.refresh_board();
                self.view = View::Board;
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-page),
            KeyCode::PageDown => self.move_selection(page),
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2),
            KeyCode::Enter => self.open_selected(),
            _ => {}
        }
    }

    fn on_key_transcript(&mut self, key: KeyEvent) {
        let page = self.transcript_height.max(1) as isize;
        match key.code {
            // The footer promises "q quit" in every view; Esc/Backspace/← are the "back" keys.
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left => {
                self.view = View::List;
            }
            KeyCode::Char('r') => {
                // Re-parse the open session from disk.
                if let Some(open) = &self.open {
                    let r = SessionRef {
                        id: open.session.id.clone(),
                        harness: open.session.harness,
                        path: open.session.source_path.clone().unwrap_or_default(),
                        cwd: open.session.cwd.clone(),
                        title: open.session.title.clone(),
                        created_at: open.session.created_at,
                        updated_at: open.session.updated_at,
                        message_count: open.session.messages.len(),
                    };
                    if let Ok(session) = load_session(&r) {
                        let lines = render_lines(&session);
                        self.open = Some(OpenSession { session, lines });
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll_transcript(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_transcript(1),
            KeyCode::PageUp => self.scroll_transcript(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_transcript(page),
            KeyCode::Home | KeyCode::Char('g') => self.transcript_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll_transcript(isize::MAX / 2),
            _ => {}
        }
    }

    fn on_key_board(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('b') => self.view = View::List,
            KeyCode::Char('r') => self.refresh_board(),
            _ => {}
        }
    }

    fn cycle_view(&mut self) {
        self.view = match self.view {
            View::List => {
                if self.open.is_some() {
                    View::Transcript
                } else {
                    self.refresh_board();
                    View::Board
                }
            }
            View::Transcript => {
                self.refresh_board();
                View::Board
            }
            View::Board => View::List,
        };
    }
}

// ───────────────────────────── free helpers ─────────────────────────────

/// Is this key plain typed text (no Ctrl/Alt chords)? Used by the filter/search input lines so
/// e.g. Ctrl-R doesn't insert a literal 'r'. Shift is fine: that's just an uppercase/shifted char.
fn is_text_input(key: KeyEvent) -> bool {
    !key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

/// Load + parse a session ref into the IR via its harness adapter.
fn load_session(r: &SessionRef) -> anyhow::Result<Session> {
    let adapter: Box<dyn Adapter> =
        harness::for_harness(r.harness).ok_or_else(|| anyhow::anyhow!("no adapter for harness {}", r.harness))?;
    adapter.parse(r)
}

/// Try semantic search if the feature/index is available; never propagate errors (best-effort).
fn try_semantic_search(q: &str) -> Option<Vec<cv_search::Hit>> {
    // `semantic_search` only exists when cv-search is built with its (default-on) `semantic`
    // feature. We call it directly; if the embedding store is missing it simply errors and we
    // fall back to the FTS-only results.
    match std::panic::catch_unwind(|| cv_search::semantic_search(None, q, 50)) {
        Ok(Ok(hits)) => Some(hits),
        _ => None,
    }
}

/// Resolve a search [`Hit`] to a [`Row`] by finding its ref among the master/all lists.
/// We discover refs once (here) so a hit always carries enough to be opened.
fn hit_to_row(h: &cv_search::Hit, app: &App) -> Option<Row> {
    // Prefer a ref we already discovered (cheap); else discover by id.
    if let Some(row) = app.all_rows.iter().find(|r| r.r#ref.id == h.id) {
        return Some(Row {
            r#ref: row.r#ref.clone(),
            snippet: Some(h.snippet.clone()),
            score: Some(h.score),
        });
    }
    let harness = Harness::parse(&h.harness)?;
    let (r, _) = cv_core::find(&h.id, Some(harness)).ok().flatten()?;
    Some(Row {
        r#ref: r,
        snippet: Some(h.snippet.clone()),
        score: Some(h.score),
    })
}

/// Does a list row match a lowercased query? Substring across harness/id/title/cwd, plus a
/// lenient subsequence ("fuzzy") fallback over the same haystack.
fn row_matches(row: &Row, q_lower: &str) -> bool {
    let r = &row.r#ref;
    let hay = format!(
        "{} {} {} {}",
        r.harness.as_str(),
        r.id,
        r.title.as_deref().unwrap_or(""),
        r.cwd.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
    )
    .to_ascii_lowercase();
    hay.contains(q_lower) || is_subsequence(q_lower, &hay)
}

/// True if every char of `needle` appears in `hay` in order (a cheap fuzzy match).
fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut hc = hay.chars();
    'outer: for nc in needle.chars() {
        if nc == ' ' {
            continue;
        }
        for h in hc.by_ref() {
            if h == nc {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

/// First 8 chars of an id, for compact display.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Replace a leading `$HOME` with `~` for compact cwd display.
pub fn tildify(path: &Path) -> String {
    let s = path.display().to_string();
    if let Some(home) = dirs::home_dir() {
        let home = home.display().to_string();
        if let Some(rest) = s.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    s
}

/// Pre-render a session into classified lines (the shape of `cv_core::render::to_plain`, but kept
/// structured so the UI can color + wrap them). Thinking and tool blocks are shortened.
fn render_lines(s: &Session) -> Vec<TLine> {
    use cv_core::ir::Block;
    let mut out: Vec<TLine> = Vec::new();

    // A small header so the reader has context.
    out.push(TLine {
        text: format!("{}  ·  {}", s.harness.as_str(), s.label()),
        kind: LineKind::Meta,
    });
    if let Some(cwd) = &s.cwd {
        out.push(TLine {
            text: format!("cwd: {}", tildify(cwd)),
            kind: LineKind::Meta,
        });
    }
    if let Some(m) = &s.model {
        out.push(TLine {
            text: format!("model: {m}"),
            kind: LineKind::Meta,
        });
    }
    out.push(TLine {
        text: String::new(),
        kind: LineKind::Meta,
    });

    for m in &s.messages {
        let (label, kind) = match m.role {
            cv_core::ir::Role::User => ("user", LineKind::RoleUser),
            cv_core::ir::Role::Assistant => ("assistant", LineKind::RoleAssistant),
            cv_core::ir::Role::System => ("system", LineKind::RoleSystem),
            cv_core::ir::Role::Tool => ("tool", LineKind::RoleTool),
        };
        out.push(TLine {
            text: format!("── {label} ──"),
            kind,
        });

        for b in &m.content {
            match b {
                Block::Text { text } => {
                    push_multiline(&mut out, text, LineKind::Text);
                }
                Block::Thinking { text, .. } => {
                    // Collapse thinking to a short preview (a few lines max).
                    let preview = preview_text(text, 6);
                    out.push(TLine {
                        text: format!("🧠 {}", preview.first().cloned().unwrap_or_default()),
                        kind: LineKind::Thinking,
                    });
                    for extra in preview.iter().skip(1) {
                        out.push(TLine {
                            text: format!("   {extra}"),
                            kind: LineKind::Thinking,
                        });
                    }
                }
                Block::ToolUse { name, input, .. } => {
                    out.push(TLine {
                        text: format!("🔧 {name}  {}", short_json(input, 200)),
                        kind: LineKind::ToolUse,
                    });
                }
                Block::ToolResult { content, is_error, .. } => {
                    let kind = if *is_error {
                        LineKind::ToolError
                    } else {
                        LineKind::ToolResult
                    };
                    let head = if *is_error { "↩ error" } else { "↩ result" };
                    out.push(TLine {
                        text: head.to_string(),
                        kind,
                    });
                    for l in preview_text(content, 12) {
                        out.push(TLine {
                            text: format!("  {l}"),
                            kind,
                        });
                    }
                }
                Block::File { path, source, .. } => {
                    let label = path.as_deref().or(source.as_deref()).unwrap_or("?");
                    out.push(TLine {
                        text: format!("[file: {label}]"),
                        kind: LineKind::Meta,
                    });
                }
                Block::Image { .. } => out.push(TLine {
                    text: "[image]".to_string(),
                    kind: LineKind::Meta,
                }),
            }
        }
        out.push(TLine {
            text: String::new(),
            kind: LineKind::Text,
        });
    }
    out
}

/// Split `text` on newlines into classified lines.
fn push_multiline(out: &mut Vec<TLine>, text: &str, kind: LineKind) {
    for line in text.split('\n') {
        out.push(TLine {
            text: line.to_string(),
            kind,
        });
    }
}

/// First `max` non-trivial lines of `text`, with a trailing "…" marker if truncated.
fn preview_text(text: &str, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    let truncated = lines.len() > max;
    lines.truncate(max);
    if truncated {
        lines.push("… (truncated)".to_string());
    }
    lines
}

/// Compact one-line JSON, truncated to `max` chars.
fn short_json(v: &impl ToString, max: usize) -> String {
    let s = v.to_string();
    if s.chars().count() <= max {
        s
    } else {
        let mut o: String = s.chars().take(max.saturating_sub(1)).collect();
        o.push('…');
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::ir::{Block, Message, Role};

    fn sref(id: &str, title: Option<&str>, cwd: Option<&str>) -> SessionRef {
        SessionRef {
            id: id.to_string(),
            harness: Harness::Claude,
            path: std::path::PathBuf::from(format!("/tmp/{id}.jsonl")),
            cwd: cwd.map(Into::into),
            title: title.map(Into::into),
            created_at: None,
            updated_at: None,
            message_count: 1,
        }
    }

    fn row(id: &str, title: Option<&str>, cwd: Option<&str>) -> Row {
        Row {
            r#ref: sref(id, title, cwd),
            snippet: None,
            score: None,
        }
    }

    fn session(messages: Vec<Message>) -> Session {
        Session {
            id: "s1".into(),
            harness: Harness::Claude,
            cwd: Some("/work/proj".into()),
            title: Some("a test session".into()),
            created_at: None,
            updated_at: None,
            model: Some("claude-test-1".into()),
            git: None,
            messages,
            source_path: None,
            extra: Default::default(),
            system_prompt: None,
            lineage: cv_core::ir::Lineage::default(),
        }
    }

    fn text_msg(role: Role, text: &str) -> Message {
        let mut m = Message::new(role);
        m.content.push(Block::Text {
            text: text.to_string().into(),
        });
        m
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // ───────────────────────── matching helpers ─────────────────────────

    #[test]
    fn subsequence_matching() {
        assert!(is_subsequence("abc", "a1b2c3"));
        assert!(is_subsequence("a c", "abc"), "spaces in the needle are skipped");
        assert!(!is_subsequence("cba", "abc"), "order matters");
        assert!(!is_subsequence("abcc", "abc"));
        assert!(is_subsequence("", "anything"));
    }

    #[test]
    fn row_matching_covers_every_field_and_fuzzy() {
        let r = row(
            "deadbeef-1234",
            Some("Fix The Parser"),
            Some("/home/em/projects/claurd"),
        );
        assert!(row_matches(&r, "claude"), "harness");
        assert!(row_matches(&r, "deadbeef"), "id");
        assert!(row_matches(&r, "fix the parser"), "title, case-folded");
        assert!(row_matches(&r, "projects/claurd"), "cwd");
        assert!(row_matches(&r, "ftp"), "subsequence fallback (f…t…p…)");
        assert!(!row_matches(&r, "zzzz"), "no match");

        let bare = row("id-only", None, None);
        assert!(row_matches(&bare, "id-only"));
        assert!(!row_matches(&bare, "title"));
    }

    // ───────────────────────── transcript rendering ─────────────────────────

    #[test]
    fn render_lines_blocks_and_kinds() {
        let mut asst = Message::new(Role::Assistant);
        asst.content.push(Block::Thinking {
            text: (0..10)
                .map(|i| format!("thought {i}"))
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
            signature: None,
            encrypted: None,
            redacted: false,
        });
        asst.content.push(Block::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "cargo test"}),
            namespace: None,
        });
        let mut toolm = Message::new(Role::Tool);
        toolm.content.push(Block::ToolResult {
            tool_use_id: "t1".into(),
            content: "error: it broke".to_string().into(),
            is_error: true,
            tool_name: None,
            status: None,
            details: None,
        });
        let s = session(vec![text_msg(Role::User, "line one\nline two"), asst, toolm]);

        let lines = render_lines(&s);

        // Header: harness · label, cwd, model.
        assert!(lines[0].text.contains("claude"), "{}", lines[0].text);
        assert!(lines[0].text.contains("a test session"));
        assert_eq!(lines[0].kind, LineKind::Meta);
        assert!(lines
            .iter()
            .any(|l| l.kind == LineKind::Meta && l.text == "cwd: /work/proj"));
        assert!(lines.iter().any(|l| l.text == "model: claude-test-1"));

        // Role separators carry role kinds.
        assert!(lines
            .iter()
            .any(|l| l.text == "── user ──" && l.kind == LineKind::RoleUser));
        assert!(lines
            .iter()
            .any(|l| l.text == "── assistant ──" && l.kind == LineKind::RoleAssistant));
        assert!(lines
            .iter()
            .any(|l| l.text == "── tool ──" && l.kind == LineKind::RoleTool));

        // Multiline text splits into one TLine per line.
        assert!(lines.iter().any(|l| l.text == "line one" && l.kind == LineKind::Text));
        assert!(lines.iter().any(|l| l.text == "line two" && l.kind == LineKind::Text));

        // Thinking collapses to 6 lines + truncation marker.
        let thinking: Vec<_> = lines.iter().filter(|l| l.kind == LineKind::Thinking).collect();
        assert_eq!(thinking.len(), 7, "6 preview lines + truncation marker");
        assert!(thinking.last().unwrap().text.contains("truncated"));

        // Tool use + error-flavored result.
        assert!(lines
            .iter()
            .any(|l| l.kind == LineKind::ToolUse && l.text.contains("Bash")));
        assert!(lines
            .iter()
            .any(|l| l.kind == LineKind::ToolError && l.text.contains("↩ error")));
        assert!(lines
            .iter()
            .any(|l| l.kind == LineKind::ToolError && l.text.contains("it broke")));
        assert!(
            !lines.iter().any(|l| l.kind == LineKind::ToolResult),
            "an error result must not also render as a plain result"
        );
    }

    #[test]
    fn preview_and_short_json_truncation() {
        assert_eq!(preview_text("a\nb", 5), vec!["a", "b"]);
        let p = preview_text("a\nb\nc\nd", 2);
        assert_eq!(p, vec!["a", "b", "… (truncated)"]);

        assert_eq!(short_json(&"short", 10), "short");
        let long = short_json(&"x".repeat(50), 10);
        assert_eq!(long.chars().count(), 10);
        assert!(long.ends_with('…'));
    }

    // ───────────────────────── selection / input state machine ─────────────────────────

    fn app_with_rows(n: usize) -> App {
        let mut app = App::new();
        app.all_rows = (0..n)
            .map(|i| row(&format!("sess-{i:02}"), Some(&format!("title {i}")), Some("/w")))
            .collect();
        app.recompute_filter();
        app
    }

    #[test]
    fn selection_clamps_and_pages() {
        let mut app = app_with_rows(5);
        app.list_height = 2;

        assert_eq!(app.selected, 0);
        app.on_key(key(KeyCode::Up)); // clamp at top
        assert_eq!(app.selected, 0);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected, 1);
        app.on_key(key(KeyCode::Char('G'))); // End: huge delta must clamp, not overflow
        assert_eq!(app.selected, 4);
        app.on_key(key(KeyCode::Down)); // clamp at bottom
        assert_eq!(app.selected, 4);
        assert!(app.list_offset > 0, "scrolled to keep the selection visible");
        app.on_key(key(KeyCode::Char('g'))); // Home
        assert_eq!(app.selected, 0);
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.selected, 2, "page = list_height");
    }

    #[test]
    fn filter_mode_narrows_and_esc_restores() {
        let mut app = app_with_rows(12);
        app.on_key(key(KeyCode::Char('/')));
        assert_eq!(app.mode, Mode::Filter);
        for c in "title 1".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        // "title 1" matches title 1, 10, 11 by substring.
        assert_eq!(app.filtered.len(), 3, "filter: {:?}", app.filter);
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.filter, "title ");
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.filter.is_empty());
        assert_eq!(app.filtered.len(), 12, "Esc restores the full list");
    }

    #[test]
    fn slash_enters_filter_even_with_shift() {
        // Some layouts deliver `/` shifted (e.g. Shift+7); the binding must not care.
        let mut app = app_with_rows(3);
        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::SHIFT));
        assert_eq!(app.mode, Mode::Filter);
        // Shifted chars are still text while filtering.
        app.on_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT));
        assert_eq!(app.filter, "T");
    }

    #[test]
    fn ctrl_c_quits_mid_filter_and_mid_search() {
        let mut app = app_with_rows(3);
        app.on_key(key(KeyCode::Char('/')));
        app.on_key(key(KeyCode::Char('a')));
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit, "Ctrl-C must quit even while typing a filter");
        assert_eq!(app.filter, "a", "…and must not be swallowed as a literal 'c'");

        let mut app = app_with_rows(3);
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.mode, Mode::Search);
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit, "Ctrl-C must quit even while typing a search");
        assert!(app.search_input.is_empty());
    }

    #[test]
    fn ctrl_and_alt_chords_are_not_text_input() {
        let mut app = app_with_rows(3);
        app.on_key(key(KeyCode::Char('/')));
        app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        assert!(app.filter.is_empty(), "chorded keys must not type into the filter");
        assert_eq!(app.mode, Mode::Filter);
    }

    #[test]
    fn q_quits_from_every_view_as_the_footer_promises() {
        // Transcript: q quits; Esc/Backspace/← go back.
        let mut app = app_with_rows(1);
        app.view = View::Transcript;
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);

        let mut app = app_with_rows(1);
        app.view = View::Board;
        app.on_key(key(KeyCode::Esc)); // back, not quit
        assert_eq!(app.view, View::List);
        assert!(!app.should_quit);
        app.view = View::Board;
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);
    }

    #[test]
    fn filter_clamps_selection_when_list_shrinks() {
        let mut app = app_with_rows(10);
        app.selected = 9;
        app.filter = "title 3".into();
        app.recompute_filter();
        assert_eq!(app.filtered.len(), 1);
        assert_eq!(app.selected, 0, "selection clamps into the shrunken list");
        assert!(app.current_row().is_some());
    }

    #[test]
    fn help_overlay_swallows_one_key() {
        let mut app = app_with_rows(3);
        app.on_key(key(KeyCode::Char('?')));
        assert_eq!(app.mode, Mode::Help);
        app.on_key(key(KeyCode::Char('j'))); // dismissed, not navigation
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn quit_keys_and_release_events() {
        let mut app = app_with_rows(1);
        // Key *release* events must be ignored (Windows/Kitty emit them).
        let mut release = key(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        app.on_key(release);
        assert!(!app.should_quit);

        app.on_key(key(KeyCode::Char('q')));
        assert!(app.should_quit);

        let mut app = app_with_rows(1);
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit, "ctrl-c quits from anywhere");
    }

    #[test]
    fn transcript_scroll_clamps() {
        let mut app = app_with_rows(1);
        let s = session(vec![text_msg(Role::User, "hi"), text_msg(Role::Assistant, "yo")]);
        let lines = render_lines(&s);
        let max = lines.len() - 1;
        app.open = Some(OpenSession { session: s, lines });
        app.view = View::Transcript;
        app.transcript_height = 4;

        app.on_key(key(KeyCode::Char('G')));
        assert_eq!(app.transcript_scroll, max, "End clamps to the last line");
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.transcript_scroll, max);
        app.on_key(key(KeyCode::Char('g')));
        assert_eq!(app.transcript_scroll, 0);
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.transcript_scroll, 0, "clamped at the top");

        // Leaving the reader goes back to the list.
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.view, View::List);
        assert!(!app.should_quit, "Esc in the reader must not quit the app");
    }
}
