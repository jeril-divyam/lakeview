//! The Miller-column browser: panes open to the right, with a detail/preview
//! pane pinned to the far right edge.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, HighlightSpacing, List, ListItem, Padding, Paragraph, Wrap,
};
use unicode_width::UnicodeWidthStr;

use super::{format_ts, human_size, justify, truncate};
use crate::app::{App, Items, Load, Mode, Pane, PreviewBody, Row, RowKind, Source};
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    app.hits.columns.clear();
    app.hits.preview = None;
    app.hits.commits = None;

    if app.mode == Mode::Zoom {
        app.hits.preview = Some(draw_detail(frame, app, area, true));
        return;
    }

    let preview_pct = app.cfg.ui.preview_percent.min(70);
    let min_col = app.cfg.ui.column_width.max(12);

    // Reserve the preview pane, but never at the cost of the focused column.
    let mut preview_w = if preview_pct == 0 {
        0
    } else {
        (area.width as u32 * preview_pct as u32 / 100) as u16
    };
    if area.width < min_col * 2 + 20 {
        preview_w = 0;
    }
    preview_w = preview_w.min(area.width.saturating_sub(min_col));

    let (columns_area, preview_area) = if preview_w > 0 {
        let [c, p] = Layout::horizontal([Constraint::Min(min_col), Constraint::Length(preview_w)])
            .areas(area);
        (c, Some(p))
    } else {
        (area, None)
    };

    if let Some(preview_area) = preview_area {
        app.hits.preview = Some(draw_detail(frame, app, preview_area, false));
    }

    // How many columns fit? Always show the focused one; drop the oldest first.
    let fit = (columns_area.width / min_col).max(1) as usize;
    let total = app.panes.len();
    let start = total.saturating_sub(fit);
    let visible = total - start;

    // Ancestors get the minimum width; the focused column keeps the remainder.
    let mut constraints: Vec<Constraint> = Vec::with_capacity(visible);
    for _ in 0..visible.saturating_sub(1) {
        constraints.push(Constraint::Length(min_col));
    }
    constraints.push(Constraint::Min(min_col));
    let chunks = Layout::horizontal(constraints).split(columns_area);

    let truncated = start > 0;
    let tick = app.tick;
    let mut hits = Vec::with_capacity(visible);
    for (slot, idx) in (start..total).enumerate() {
        let focused = idx == total - 1;
        let elided = truncated && slot == 0;
        let list_area = draw_column(
            frame,
            &mut app.panes[idx],
            chunks[slot],
            focused,
            elided,
            tick,
        );
        hits.push((idx, list_area));
    }
    app.hits.columns = hits;
}

/// Renders one column and returns the inner area its rows occupy, so mouse
/// clicks can be mapped back to list rows.
fn draw_column(
    frame: &mut Frame,
    pane: &mut Pane,
    area: Rect,
    focused: bool,
    elided: bool,
    tick: usize,
) -> Rect {
    let count = pane.rows.len();
    let mut title = vec![
        Span::raw(" "),
        Span::styled(
            truncate(&pane.title(), area.width.saturating_sub(8) as usize),
            Theme::title(focused),
        ),
        Span::raw(" "),
    ];
    if elided {
        // Signal that older columns are scrolled off to the left.
        title.insert(0, Span::styled("‹", Theme::faint()));
    }

    let mut right = Vec::new();
    if !pane.filter.is_empty() {
        right.push(Span::styled(
            format!(" /{} ", truncate(&pane.filter, 8)),
            Theme::accent(),
        ));
    }
    if matches!(pane.load, Load::Ready) && count > 0 {
        right.push(Span::styled(format!(" {count} "), Theme::faint()));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Theme::border(focused))
        .title_top(Line::from(title))
        .title_top(Line::from(right).right_aligned());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    match &pane.load {
        Load::Loading => {
            frame.render_widget(
                centered_note(
                    &format!("{} loading…", super::SPINNER[(tick / 2) % 8]),
                    Theme::dim(),
                ),
                inner,
            );
            return inner;
        }
        Load::Failed(err) => {
            frame.render_widget(
                Paragraph::new(Text::from(err.as_str()))
                    .style(Theme::error())
                    .wrap(Wrap { trim: true }),
                inner,
            );
            return inner;
        }
        Load::Ready => {}
    }

    if pane.rows.is_empty() {
        let note = if pane.filter.is_empty() {
            "empty"
        } else {
            "no matches"
        };
        frame.render_widget(centered_note(note, Theme::faint()), inner);
        return inner;
    }

    // Two columns are consumed by the highlight bar and its trailing space.
    let width = inner.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = pane
        .rows
        .iter()
        .map(|row| ListItem::new(render_row(row, width)))
        .collect();

    let list = List::new(items)
        .highlight_style(Theme::selection(focused))
        .highlight_symbol(if focused { "▌" } else { " " })
        .highlight_spacing(HighlightSpacing::Always)
        .scroll_padding(2);

    frame.render_stateful_widget(list, inner, &mut pane.state);
    inner
}

fn render_row(row: &Row, width: usize) -> Line<'static> {
    let (icon, style) = match row.kind {
        RowKind::Repo => ("◆ ", Style::new().fg(Theme::ACCENT)),
        RowKind::Branch if row.primary => (
            "● ",
            Style::new().fg(Theme::GREEN).add_modifier(Modifier::BOLD),
        ),
        RowKind::Branch => ("○ ", Style::new().fg(Theme::FG)),
        RowKind::Tag => ("◇ ", Style::new().fg(Theme::PURPLE)),
        RowKind::Dir => ("▸ ", Theme::directory()),
        RowKind::File => ("  ", Theme::file()),
    };

    let label = if row.kind == RowKind::Dir {
        format!("{}/", row.label)
    } else {
        row.label.clone()
    };

    let body_width = width.saturating_sub(icon.width());
    let (label, pad) = justify(&label, &row.meta, body_width);

    Line::from(vec![
        Span::styled(icon, style),
        Span::styled(label, style),
        Span::raw(pad),
        Span::styled(row.meta.clone(), Theme::faint()),
    ])
}

fn centered_note(text: &str, style: Style) -> Paragraph<'static> {
    Paragraph::new(Line::styled(text.to_string(), style))
        .centered()
        .block(Block::default().padding(Padding::top(1)))
}

// ── detail / preview pane ────────────────────────────────────────────────

/// Renders the detail/preview pane and returns its inner area.
fn draw_detail(frame: &mut Frame, app: &App, area: Rect, zoomed: bool) -> Rect {
    let pane = app.focused();
    let heading = pane
        .selected_row()
        .map(|r| r.label.clone())
        .unwrap_or_else(|| "Details".into());

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Theme::border(zoomed))
        .padding(Padding::horizontal(1))
        .title_top(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                truncate(&heading, area.width.saturating_sub(10) as usize),
                Theme::title(zoomed),
            ),
            Span::raw(" "),
        ]));

    let mut right = Vec::new();
    if zoomed {
        right.push(Span::styled(" zoom ", Theme::chip()));
        right.push(Span::raw(" "));
    }
    let block = block.title_top(Line::from(right).right_aligned());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    match pane.selected_row() {
        None => lines.push(Line::styled("nothing selected", Theme::faint())),
        Some(row) => match (&pane.source, &pane.items) {
            (Source::Repos, Items::Repos(v)) => {
                if let Some(repo) = v.get(row.index) {
                    lines.push(kv("repository", &repo.id, Theme::accent()));
                    lines.push(kv(
                        "default branch",
                        &repo.default_branch,
                        Style::new().fg(Theme::GREEN),
                    ));
                    lines.push(kv("created", &format_ts(repo.creation_date), Theme::file()));
                    lines.push(Line::raw(""));
                    lines.push(Line::styled("storage namespace", Theme::faint()));
                    lines.push(Line::styled(repo.storage_namespace.clone(), Theme::dim()));
                }
            }
            (Source::Refs { .. }, Items::Refs(v)) => {
                if let Some(r) = v.get(row.index) {
                    let kind = match r.kind {
                        crate::lakefs::RefKind::Branch => "branch",
                        crate::lakefs::RefKind::Tag => "tag",
                    };
                    lines.push(kv("name", &r.id, Theme::accent()));
                    lines.push(kv("type", kind, Theme::file()));
                    if r.is_default {
                        lines.push(kv("default", "yes", Style::new().fg(Theme::GREEN)));
                    }
                    lines.push(Line::raw(""));
                    lines.push(Line::styled("head commit", Theme::faint()));
                    lines.push(Line::styled(r.commit_id.clone(), Theme::dim()));
                    lines.push(Line::raw(""));
                    lines.push(Line::styled(
                        "press → to browse objects, 2 for the commit log",
                        Theme::faint(),
                    ));
                }
            }
            (Source::Objects { .. }, Items::Objects(v)) => {
                if let Some(obj) = v.get(row.index) {
                    if obj.is_dir() {
                        lines.push(kv("prefix", obj.name(), Theme::directory()));
                        lines.push(Line::raw(""));
                        lines.push(Line::styled(obj.path.clone(), Theme::dim()));
                        lines.push(Line::raw(""));
                        lines.push(Line::styled("press → to open", Theme::faint()));
                    } else {
                        lines.extend(object_details(obj));
                        lines.push(Line::raw(""));
                        lines.extend(preview_lines(app, inner.width));
                    }
                }
            }
            _ => {}
        },
    }

    let paragraph = Paragraph::new(Text::from(lines)).scroll((app.preview.scroll, 0));
    frame.render_widget(paragraph, inner);
    inner
}

fn object_details(obj: &crate::lakefs::ObjectStats) -> Vec<Line<'static>> {
    let mut lines = vec![
        kv(
            "size",
            &human_size(obj.size_bytes.unwrap_or(0)),
            Theme::file(),
        ),
        kv("modified", &format_ts(obj.mtime), Theme::file()),
    ];
    if let Some(ct) = &obj.content_type
        && !ct.is_empty()
    {
        lines.push(kv("type", ct, Theme::file()));
    }
    if !obj.checksum.is_empty() {
        lines.push(kv("checksum", &truncate(&obj.checksum, 32), Theme::dim()));
    }
    if !obj.physical_address.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled("physical address", Theme::faint()));
        lines.push(Line::styled(obj.physical_address.clone(), Theme::dim()));
    }
    lines
}

fn preview_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::styled("─".repeat(width as usize), Theme::faint()));

    if app.preview.loading {
        lines.push(Line::styled("loading preview…", Theme::dim()));
        return lines;
    }
    if let Some(err) = &app.preview.error {
        lines.push(Line::styled(err.clone(), Theme::error()));
        return lines;
    }

    match &app.preview.body {
        None => lines.push(Line::styled("no preview", Theme::faint())),
        Some(PreviewBody::Binary(rows)) => {
            lines.push(Line::styled("binary — hex dump", Theme::faint()));
            lines.push(Line::raw(""));
            lines.extend(rows.iter().map(|r| Line::styled(r.clone(), Theme::dim())));
        }
        Some(PreviewBody::Text(rows)) if rows.is_empty() => {
            lines.push(Line::styled("empty file", Theme::faint()));
        }
        Some(PreviewBody::Json(rows)) => {
            let gutter = rows.len().to_string().len().max(2);
            for (i, row) in rows.iter().enumerate() {
                let mut spans = vec![Span::styled(format!("{:>gutter$} ", i + 1), Theme::faint())];
                spans.extend(
                    row.iter()
                        .map(|(tok, text)| Span::styled(text.clone(), Theme::json(*tok))),
                );
                lines.push(Line::from(spans));
            }
        }
        Some(PreviewBody::Text(rows)) => {
            let gutter = rows.len().to_string().len().max(2);
            for (i, row) in rows.iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(format!("{:>gutter$} ", i + 1), Theme::faint()),
                    Span::styled(row.clone(), Theme::file()),
                ]));
            }
        }
    }
    lines
}

/// `key   value` with a dim label column.
fn kv(key: &str, value: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<16}"), Theme::faint()),
        Span::styled(value.to_string(), style),
    ])
}
