//! Rendering. The frame is header / breadcrumb / body / footer, where the
//! body is tab-dependent.

mod browse;
mod commits;
mod help;
mod overlay;

use chrono::{Local, TimeZone};
use humansize::{DECIMAL, format_size};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Mode, Tab};
use crate::theme::Theme;

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    // Paint the whole canvas so the theme background wins over the terminal's.
    frame.render_widget(Block::new().style(Theme::base()), area);

    let [header, crumbs, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(frame, app, header);
    draw_breadcrumb(frame, app, crumbs);

    match app.tab {
        Tab::Browse => browse::draw(frame, app, body),
        Tab::Commits => commits::draw(frame, app, body),
        Tab::Help => help::draw(frame, body),
    }

    draw_footer(frame, app, footer);

    if let Mode::Profiles(selected) = app.mode {
        overlay::profiles(frame, app, selected, area);
    }
}

fn draw_header(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Theme::border(false));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Tabs, numbered like the reference layout: "1 Browse  2 Commits".
    let mut spans = vec![Span::raw(" ")];
    let mut tabs_width = 1usize;
    let mut x = inner.x + 1;
    app.hits.tabs.clear();
    for (i, tab) in Tab::ALL.iter().enumerate() {
        let label = format!(" {} {} ", i + 1, tab.label());
        let width = label.width() as u16;
        if x + width <= inner.x + inner.width {
            app.hits.tabs.push((*tab, Rect::new(x, inner.y, width, 1)));
        }
        x += width + 1;
        tabs_width += label.width() + 1;
        let style = if *tab == app.tab {
            Theme::chip()
        } else {
            Theme::dim()
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);

    // Right side: spinner, profile name and endpoint — dropped piecewise as
    // the terminal narrows so it never collides with the tabs.
    let spinner_width = if app.inflight > 0 { 2 } else { 0 };
    let room = (inner.width as usize).saturating_sub(tabs_width + spinner_width + 1);
    let endpoint = app.profile.endpoint.trim_end_matches('/');

    let mut right = Vec::new();
    if app.inflight > 0 {
        right.push(Span::styled(
            format!("{} ", SPINNER[(app.tick / 2) % SPINNER.len()]),
            Theme::accent(),
        ));
    }
    if room >= app.profile_name.width() + endpoint.width() + 3 {
        right.push(Span::styled(&app.profile_name, Theme::accent()));
        right.push(Span::styled(" · ", Theme::faint()));
        right.push(Span::styled(endpoint, Theme::dim()));
    } else if room >= app.profile_name.width() {
        right.push(Span::styled(&app.profile_name, Theme::accent()));
    } else {
        right.clear();
    }

    if !right.is_empty() {
        right.push(Span::raw(" "));
        frame.render_widget(
            Paragraph::new(Line::from(right)).alignment(Alignment::Right),
            inner,
        );
    }
}

fn draw_breadcrumb(frame: &mut Frame, app: &App, area: Rect) {
    let parts = app.breadcrumb();
    let mut spans = vec![Span::styled(" lakefs://", Theme::faint())];
    if parts.is_empty() {
        spans.push(Span::styled("(all repositories)", Theme::faint()));
    }
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("/", Theme::faint()));
        }
        let style = match i {
            0 => Theme::accent(),
            1 => Style::new().fg(Theme::GREEN),
            _ => Style::new().fg(Theme::FG),
        };
        spans.push(Span::styled(part.clone(), style));
    }

    // Filter indicator for the focused pane.
    let filter = &app.focused().filter;
    if !filter.is_empty() || app.mode == Mode::Filter {
        spans.push(Span::styled("   filter: ", Theme::faint()));
        spans.push(Span::styled(filter.clone(), Theme::accent()));
        if app.mode == Mode::Filter {
            spans.push(Span::styled("█", Theme::accent()));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(status) = &app.status {
        let style = if status.is_error {
            Theme::error()
        } else {
            Theme::dim()
        };
        let icon = if status.is_error { " ✗ " } else { " · " };
        let line = Line::from(vec![
            Span::styled(icon, style),
            Span::styled(status.text.clone(), style),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    let hints: &[(&str, &str)] = match (app.tab, &app.mode) {
        (_, Mode::Filter) => &[("type", "filter"), ("Enter", "apply"), ("Esc", "clear")],
        (_, Mode::Profiles(_)) => &[("↑↓/jk", "move"), ("Enter", "switch"), ("Esc", "cancel")],
        (_, Mode::Zoom) => &[("↑↓/jk", "scroll"), ("Esc/h", "back"), ("y", "copy uri")],
        (Tab::Browse, _) => &[
            ("↑↓/jk", "move"),
            ("→/l", "open"),
            ("←/h", "back"),
            ("/", "filter"),
            ("y", "copy"),
            ("r", "reload"),
            ("p", "profile"),
            ("q", "quit"),
        ],
        (Tab::Commits, _) => &[
            ("↑↓/jk", "move"),
            ("r", "reload"),
            ("Tab", "switch"),
            ("q", "quit"),
        ],
        (Tab::Help, _) => &[("Tab", "switch tab"), ("q", "quit")],
    };

    let mut spans = Vec::new();
    for (key, label) in hints {
        spans.push(Span::styled(format!(" {key} "), Theme::chip()));
        spans.push(Span::styled(format!(" {label}  "), Theme::dim()));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}

// ── shared helpers ───────────────────────────────────────────────────────

pub fn human_size(bytes: u64) -> String {
    format_size(bytes, DECIMAL)
}

pub fn format_ts(epoch_secs: i64) -> String {
    if epoch_secs <= 0 {
        return "—".into();
    }
    match Local.timestamp_opt(epoch_secs, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => "—".into(),
    }
}

/// Relative age, e.g. "3d ago" — used in the commit log.
pub fn relative_age(epoch_secs: i64) -> String {
    if epoch_secs <= 0 {
        return "—".into();
    }
    let now = chrono::Utc::now().timestamp();
    let delta = (now - epoch_secs).max(0);
    match delta {
        d if d < 60 => "just now".into(),
        d if d < 3600 => format!("{}m ago", d / 60),
        d if d < 86_400 => format!("{}h ago", d / 3600),
        d if d < 2_592_000 => format!("{}d ago", d / 86_400),
        d if d < 31_536_000 => format!("{}mo ago", d / 2_592_000),
        d => format!("{}y ago", d / 31_536_000),
    }
}

/// Truncate to `width` display columns, adding an ellipsis when cut.
pub fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "…".into();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + w > width - 1 {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Build a `label ........ meta` line filling exactly `width` columns.
pub fn justify(label: &str, meta: &str, width: usize) -> (String, String) {
    let meta_w = meta.width();
    let room = width.saturating_sub(meta_w + 1);
    let label = truncate(label, room.max(1));
    let pad = width.saturating_sub(label.width() + meta_w).max(1);
    (label, " ".repeat(pad))
}
