//! Rendering: a pure projection of [`App`] onto a ratatui [`Frame`].
//!
//! `draw` dispatches on the current view, lays out a header / body / footer, and (for the list and
//! transcript) writes back the visible height onto `App` so paging math in `app.rs` stays accurate.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use cv_core::ir::Harness;

use crate::app::{short_id, tildify};
use crate::app::{App, LineKind, Mode, Row, View};

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    // header (1) / body (rest) / footer (1).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    draw_header(f, app, chunks[0]);

    match app.view {
        View::List => draw_list_view(f, app, chunks[1]),
        View::Transcript => draw_transcript(f, app, chunks[1]),
        View::Board => draw_board(f, app, chunks[1]),
    }

    draw_footer(f, app, chunks[2]);

    if app.mode == Mode::Help {
        draw_help_overlay(f, area);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let title = match app.view {
        View::List if app.showing_search => " 🔮 clustervision · search results ",
        View::List => " 🔮 clustervision · sessions ",
        View::Transcript => " 🔮 clustervision · transcript ",
        View::Board => " 🔮 clustervision · board ",
    };
    let p = Paragraph::new(Line::from(vec![Span::styled(
        title,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    )]))
    .style(Style::default().bg(Color::Magenta));
    f.render_widget(p, area);
}

// ───────────────────────────── list view ─────────────────────────────

fn draw_list_view(f: &mut Frame, app: &mut App, area: Rect) {
    // When something is open, show a split: list on the left, transcript preview on the right.
    // Otherwise the list takes the full width.
    if app.open.is_some() {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(area);
        draw_list(f, app, cols[0]);
        draw_transcript_inner(f, app, cols[1], "preview (Enter/→ to focus)");
    } else {
        draw_list(f, app, area);
    }
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    // Reserve a row for the `/` filter line if filtering.
    let (list_area, filter_area) = if app.mode == Mode::Filter || !app.filter.is_empty() {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        (rows[0], Some(rows[1]))
    } else {
        (area, None)
    };

    let inner_h = list_area.height.saturating_sub(2) as usize; // minus borders
    app.list_height = inner_h.max(1);

    let items: Vec<ListItem> = app.filtered.iter().map(|&i| row_to_item(&app.all_rows[i])).collect();

    let count = app.filtered.len();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" sessions ({count}) "));

    let mut state = ListState::default();
    if count > 0 {
        state.select(Some(app.selected.min(count - 1)));
    }
    *state.offset_mut() = app.list_offset.min(count.saturating_sub(1));

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, list_area, &mut state);

    if let Some(fa) = filter_area {
        let cursor = if app.mode == Mode::Filter { "▏" } else { "" };
        let line = Line::from(vec![
            Span::styled("/", Style::default().fg(Color::Yellow)),
            Span::raw(app.filter.clone()),
            Span::styled(cursor, Style::default().fg(Color::Yellow)),
        ]);
        f.render_widget(Paragraph::new(line), fa);
    }
}

fn row_to_item(row: &Row) -> ListItem<'static> {
    let r = &row.r#ref;
    let hcolor = harness_color(r.harness);
    let date = r
        .updated_at
        .or(r.created_at)
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "—".to_string());
    let title = r.title.clone().unwrap_or_else(|| "(untitled)".to_string());
    let title = truncate(&title, 48);
    let cwd = r.cwd.as_ref().map(|p| tildify(p)).unwrap_or_default();

    let mut spans = vec![
        Span::styled(
            format!("{:<10}", r.harness.as_str()),
            Style::default().fg(hcolor).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{:<9}", short_id(&r.id)), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{date}  "), Style::default().fg(Color::Cyan)),
        Span::raw(title),
    ];
    if !cwd.is_empty() {
        spans.push(Span::styled(format!("  {cwd}"), Style::default().fg(Color::Green)));
    }

    let mut lines = vec![Line::from(spans)];
    if let Some(snippet) = &row.snippet {
        let score = row.score.map(|s| format!("[{s:.2}] ")).unwrap_or_default();
        lines.push(Line::from(Span::styled(
            format!("    {score}{}", truncate(snippet, 90)),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        )));
    }
    ListItem::new(lines)
}

// ───────────────────────────── transcript ─────────────────────────────

fn draw_transcript(f: &mut Frame, app: &mut App, area: Rect) {
    draw_transcript_inner(f, app, area, "transcript");
}

fn draw_transcript_inner(f: &mut Frame, app: &mut App, area: Rect, title: &str) {
    let inner_h = area.height.saturating_sub(2) as usize;
    app.transcript_height = inner_h.max(1);

    let Some(open) = &app.open else {
        let p = Paragraph::new("No session open. Select one and press Enter.")
            .block(Block::default().borders(Borders::ALL).title(format!(" {title} ")))
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, area);
        return;
    };

    let total = open.lines.len();
    let scroll = app.transcript_scroll.min(total.saturating_sub(1));

    let text: Vec<Line> = open.lines.iter().map(|tl| line_to_styled(&tl.text, tl.kind)).collect();

    let pct = if total <= 1 {
        100
    } else {
        ((scroll + inner_h).min(total) * 100 / total).min(100)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} — {}  ({pct}%) ", short_id(&open.session.id)));

    let p = Paragraph::new(Text::from(text))
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));
    f.render_widget(p, area);
}

fn line_to_styled(text: &str, kind: LineKind) -> Line<'static> {
    let style = match kind {
        LineKind::RoleUser => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        LineKind::RoleAssistant => Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        LineKind::RoleSystem => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        LineKind::RoleTool => Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
        LineKind::Text => Style::default().fg(Color::White),
        LineKind::Thinking => Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        LineKind::ToolUse => Style::default().fg(Color::Cyan),
        LineKind::ToolResult => Style::default().fg(Color::Blue),
        LineKind::ToolError => Style::default().fg(Color::Red),
        LineKind::Meta => Style::default().fg(Color::DarkGray),
    };
    Line::from(Span::styled(text.to_string(), style))
}

// ───────────────────────────── board ─────────────────────────────

fn draw_board(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(area);

    // Messages.
    let items: Vec<ListItem> = app
        .board_msgs
        .iter()
        .rev() // newest at top
        .map(|m| {
            let ts = m.ts.format("%m-%d %H:%M").to_string();
            let kind_color = match m.kind.as_str() {
                "status" => Color::Cyan,
                "event" | "claim" => Color::Yellow,
                "request" => Color::Magenta,
                "reply" | "ack" => Color::Green,
                "presence" => Color::DarkGray,
                _ => Color::White,
            };
            Line::from(vec![
                Span::styled(format!("{ts} "), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{:<8} ", truncate(&m.kind, 8)),
                    Style::default().fg(kind_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{}: ", truncate(&m.from, 16)), Style::default().fg(Color::Blue)),
                Span::raw(truncate(&m.body, 80)),
            ])
        })
        .map(ListItem::new)
        .collect();
    let msgs = List::new(items).block(Block::default().borders(Borders::ALL).title(format!(
        " board: {} ({}) ",
        app.board_channel,
        app.board_msgs.len()
    )));
    f.render_widget(msgs, cols[0]);

    // Active claims.
    let claim_items: Vec<ListItem> = app
        .board_claims
        .iter()
        .map(|(key, owner, exp)| {
            Line::from(vec![
                Span::styled(truncate(key, 28), Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(truncate(owner, 16), Style::default().fg(Color::Green)),
                Span::styled(
                    format!("  ⌛{}", exp.format("%H:%M")),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .map(ListItem::new)
        .collect();
    let claims = List::new(claim_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" active claims ({}) ", app.board_claims.len())),
    );
    f.render_widget(claims, cols[1]);
}

// ───────────────────────────── footer + help ─────────────────────────────

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    // While filtering, make the mode unmistakable: show a FILTER badge, the live match count,
    // and the keys that apply (the input line itself lives just under the list).
    if app.mode == Mode::Filter {
        let line = Line::from(vec![
            Span::styled(
                " FILTER ",
                Style::default()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}/{} match", app.filtered.len(), app.all_rows.len()),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                "  ·  type to narrow · ↑↓ move · Enter open · Esc clear & exit",
                Style::default().fg(Color::Gray),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    // While typing search, the footer becomes the input line.
    if app.mode == Mode::Search {
        let line = Line::from(vec![
            Span::styled(" search: ", Style::default().bg(Color::Blue).fg(Color::Black)),
            Span::raw(format!(" {}", app.search_input)),
            Span::styled("▏", Style::default().fg(Color::Yellow)),
        ]);
        f.render_widget(Paragraph::new(line).style(Style::default().bg(Color::Black)), area);
        return;
    }

    let keys = match app.view {
        View::List => "↑↓ move · / filter · s search · Enter open · b board · r refresh · ? help · q quit",
        View::Transcript => "↑↓ scroll · PgUp/PgDn · g/G top/bot · Esc back · r reload · ? help · q quit",
        View::Board => "r refresh · b/Esc back · ? help · q quit",
    };
    let status = app.status.clone().unwrap_or_default();
    let line = Line::from(vec![
        Span::styled(format!(" {keys} "), Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    let popup = centered_rect(64, 70, area);
    f.render_widget(Clear, popup);

    let help = vec![
        Line::from(Span::styled(
            "clustervision TUI — keybindings",
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  Global"),
        Line::from("    Tab        cycle list → transcript → board"),
        Line::from("    ?          toggle this help"),
        Line::from("    q          quit   ·   Ctrl-C quit (even mid-filter/search)"),
        Line::from("    Esc        back (in the list: quit)"),
        Line::from("    r          refresh current view"),
        Line::from(""),
        Line::from("  Session list"),
        Line::from("    ↑/↓ j/k    move        PgUp/PgDn   page"),
        Line::from("    g / G      top / bottom"),
        Line::from("    /          filter (substring + fuzzy)"),
        Line::from("    s          full-text + semantic search"),
        Line::from("    Enter      open transcript"),
        Line::from("    b          open the fleet board"),
        Line::from(""),
        Line::from("  Transcript"),
        Line::from("    ↑/↓ j/k    scroll      PgUp/PgDn/Space page"),
        Line::from("    g / G      top / bottom"),
        Line::from("    Esc/←      back to list"),
        Line::from(""),
        Line::from(Span::styled(
            "  press any key to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help ")
        .style(Style::default().bg(Color::Black));
    f.render_widget(Paragraph::new(help).block(block), popup);
}

// ───────────────────────────── tiny helpers ─────────────────────────────

fn harness_color(h: Harness) -> Color {
    match h {
        Harness::Claude => Color::Rgb(217, 119, 87), // claude orange
        Harness::ClaudeApp => Color::Rgb(217, 150, 120),
        Harness::Codex => Color::White,
        Harness::ChatGptApp => Color::Rgb(116, 170, 156),
        Harness::Grok => Color::Rgb(120, 120, 200),
        Harness::OpenCode => Color::Yellow,
        Harness::Gemini => Color::Rgb(90, 150, 240),
        Harness::Hermes => Color::Magenta,
        Harness::OpenClaw => Color::Rgb(200, 120, 60),
        Harness::Cursor => Color::Rgb(160, 160, 160),
        Harness::Kimi => Color::Rgb(80, 200, 160),
        Harness::KimiCode => Color::Rgb(60, 170, 190),
        Harness::Qwen => Color::Rgb(150, 90, 200),
        Harness::LmStudio => Color::Rgb(100, 180, 120),
        Harness::Cline => Color::Rgb(90, 180, 200),
        Harness::Roo => Color::Rgb(200, 90, 140),
        Harness::Continue => Color::Rgb(120, 200, 90),
        Harness::Goose => Color::Rgb(180, 160, 90),
        Harness::Zed => Color::Rgb(7, 81, 207),              // zed accent blue
        Harness::ChatGptExport => Color::Rgb(116, 170, 156), // chatgpt teal (export)
        Harness::ClaudeExport => Color::Rgb(217, 119, 87),   // claude orange (export)
        Harness::OpenSession => Color::Rgb(109, 73, 214),    // --h-opensession, the web UI's violet
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let mut o: String = s.chars().take(max.saturating_sub(1)).collect();
        o.push('…');
        o
    }
}

/// A centered rect that is `pct_x`% wide and `pct_y`% tall within `r`.
fn centered_rect(pct_x: u16, pct_y: u16, r: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}
