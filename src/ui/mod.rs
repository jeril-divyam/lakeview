//! Rendering. The frame is header / body / footer, where the body is
//! tab-dependent. The header carries the tabs on the left and the profile on the
//! right, with the filter you are typing in the gap between them.

mod browse;
mod commits;
mod help;
mod overlay;

// The pane floors, which `App` clamps a dragged border against. They live beside
// the layout that enforces them, but the collapse rule they feed is policy and
// belongs with the rest of the mouse handling.
pub(crate) use browse::{MIN_PREVIEW, MIN_REPOS, MIN_TREE};

use chrono::{Local, TimeZone};
use humansize::{DECIMAL, format_size};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
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

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(frame, app, header);

    match app.tab {
        Tab::Browse => browse::draw(frame, app, body),
        Tab::Commits => commits::draw(frame, app, body),
        Tab::Help => help::draw(frame, body),
    }

    draw_footer(frame, app, footer);

    if let Mode::Profiles(selected) = app.mode {
        overlay::profiles(frame, app, selected, area);
    }
    // Inside the zoom's own frame rather than over the whole screen: the menu
    // belongs to the file it filters, and it clears a band around itself, which
    // must not take the pane's borders or its title with it.
    if app.mode == Mode::Keys {
        let area = app.hits.preview.unwrap_or(body);
        overlay::keys(frame, app, area);
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
    let busy = app.busy();
    let spinner_width = if busy { 2 } else { 0 };
    let room = (inner.width as usize).saturating_sub(tabs_width + spinner_width + 1);
    let endpoint = app.profile.endpoint.trim_end_matches('/');

    let mut right = Vec::new();
    if busy {
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

    let mut right_width = 0;
    if !right.is_empty() {
        right.push(Span::raw(" "));
        right_width = right.iter().map(|s| s.content.width()).sum();
        frame.render_widget(
            Paragraph::new(Line::from(right)).alignment(Alignment::Right),
            inner,
        );
    }

    if app.mode == Mode::Filter {
        draw_filter(frame, app.filter(), inner, tabs_width, right_width);
    }
}

/// The filter being typed, in the gap between the tabs and the profile. The
/// panes annotate their own titles with the needle once it is applied; this is
/// the live one, with a cursor, and it only shows while you are typing.
fn draw_filter(
    frame: &mut Frame,
    filter: &str,
    inner: Rect,
    tabs_width: usize,
    right_width: usize,
) {
    let total = inner.width as usize;
    let room = total.saturating_sub(tabs_width + right_width + 4);
    // "filter: " and the cursor leave nothing worth showing below this.
    if room < 12 {
        return;
    }

    let line = filter_line(&truncate(filter, room - 9));
    let width = line_width(&line);
    // Centred where the width allows, nudged aside where it doesn't — a centred
    // line long enough to reach the tabs would be drawn straight over them.
    let centred = total.saturating_sub(width) / 2;
    let leftmost = tabs_width + 2;
    let rightmost = total.saturating_sub(right_width + 2 + width);
    let x = centred.clamp(leftmost, rightmost.max(leftmost));

    let slot = Rect {
        x: inner.x + x as u16,
        y: inner.y,
        width: width.min(total.saturating_sub(x)) as u16,
        height: 1,
    };
    frame.render_widget(Paragraph::new(line), slot);
}

fn filter_line(filter: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("filter: ", Theme::faint()),
        Span::styled(filter.to_string(), Theme::accent()),
        Span::styled("█", Theme::accent()),
    ])
}

fn line_width(line: &Line) -> usize {
    line.spans.iter().map(|s| s.content.width()).sum()
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
        (_, Mode::Keys) => &[
            ("↑↓/jk", "move"),
            ("space", "on/off"),
            ("←→", "fold"),
            ("a/n", "all/none"),
            ("Enter", "apply"),
            ("Esc", "cancel"),
        ],
        // Only a JSONL file is a shape repeated often enough for its keys to be
        // worth switching off, so only it offers the menu.
        (_, Mode::Zoom) if app.jsonl().is_some() => &[
            ("↑↓/jk", "move"),
            ("→/space", "unfold"),
            ("←/h", "fold"),
            ("a/c", "all"),
            ("F", "filter"),
            ("Esc", "leave"),
            ("d", "download"),
        ],
        // A zoomed JSON file has rows to fold; anything else zoomed is a flat
        // body, where the same keys only move the view.
        (_, Mode::Zoom) if app.zoom_doc().is_some() => &[
            ("↑↓/jk", "move"),
            ("→/space", "unfold"),
            ("←/h", "fold"),
            ("a/c", "all"),
            ("Esc", "leave"),
            ("d", "download"),
        ],
        (_, Mode::Zoom) => &[("↑↓/jk", "scroll"), ("Esc", "back"), ("d", "download")],
        (Tab::Browse, _) => &[
            ("↑↓/jk", "move"),
            ("→/l", "expand"),
            ("⏎", "open"),
            ("←/h", "back"),
            ("space", "toggle"),
            ("/", "search"),
            ("d", "download"),
            ("r", "reload"),
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
