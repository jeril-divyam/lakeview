//! Commit log for the repository/ref currently open in the browser.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, HighlightSpacing, List, ListItem, Padding, Paragraph, Wrap,
};

use super::{format_ts, relative_age, truncate};
use crate::app::{App, Load};
use crate::theme::Theme;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(area);

    let title = match app.context() {
        Some((repo, reference)) => format!("{repo} @ {reference}"),
        None => "Commits".to_string(),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Theme::border(true))
        .padding(Padding::top(1))
        .title_top(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                truncate(&title, list_area.width.saturating_sub(6) as usize),
                Theme::title(true),
            ),
            Span::raw(" "),
        ]));
    let inner = block.inner(list_area);
    frame.render_widget(block, list_area);
    app.hits.commits = Some(inner);
    app.hits.repos = None;
    app.hits.tree = None;
    app.hits.preview = None;
    app.hits.dividers.clear();

    match &app.commits.load {
        // `Idle` never reaches here — the commits view uses `None` for that.
        None | Some(Load::Idle) => {
            frame.render_widget(
                note("open a repository and ref in the Browse tab first"),
                inner,
            );
        }
        Some(Load::Loading) => {
            frame.render_widget(
                note(&format!(
                    "{} loading commits…",
                    super::SPINNER[(app.tick / 2) % 8]
                )),
                inner,
            );
        }
        Some(Load::Failed(err)) => {
            frame.render_widget(
                Paragraph::new(Text::from(err.as_str()))
                    .style(Theme::error())
                    .wrap(Wrap { trim: true })
                    .block(Block::default().padding(Padding::horizontal(1))),
                inner,
            );
        }
        Some(Load::Ready) if app.commits.commits.is_empty() => {
            frame.render_widget(note("no commits"), inner);
        }
        Some(Load::Ready) => {
            let width = inner.width.saturating_sub(2) as usize;
            let items: Vec<ListItem> = app
                .commits
                .commits
                .iter()
                .map(|c| {
                    let age = relative_age(c.creation_date);
                    let head = format!("{}  ", c.short_id());
                    let room = width.saturating_sub(head.len() + age.len() + 2);
                    let summary = truncate(c.summary(), room.max(4));
                    let pad = width
                        .saturating_sub(head.len() + summary.chars().count() + age.len())
                        .max(1);
                    ListItem::new(Line::from(vec![
                        Span::styled(head, Theme::accent()),
                        Span::styled(summary, Theme::file()),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(age, Theme::faint()),
                    ]))
                })
                .collect();

            let list = List::new(items)
                .highlight_style(Theme::selection(true))
                .highlight_symbol("▌")
                .highlight_spacing(HighlightSpacing::Always)
                .scroll_padding(2);
            frame.render_stateful_widget(list, inner, &mut app.commits.state);
        }
    }

    draw_commit_detail(frame, app, detail_area);
}

fn draw_commit_detail(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Theme::border(false))
        .padding(Padding::new(1, 1, 1, 0))
        .title_top(Line::from(vec![
            Span::raw(" "),
            Span::styled("Commit", Theme::title(false)),
            Span::raw(" "),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(commit) = app
        .commits
        .state
        .selected()
        .and_then(|i| app.commits.commits.get(i))
    else {
        return;
    };

    let mut lines = vec![
        Line::styled(commit.summary().to_string(), Theme::accent_bold()),
        Line::raw(""),
        kv("commit", &commit.id),
        kv("committer", &commit.committer),
        kv("date", &format_ts(commit.creation_date)),
    ];
    for (i, parent) in commit.parents.iter().enumerate() {
        let label = if i == 0 { "parent" } else { "" };
        lines.push(kv(label, parent));
    }

    let body: Vec<&str> = commit.message.lines().skip(1).collect();
    if body.iter().any(|l| !l.trim().is_empty()) {
        lines.push(Line::raw(""));
        for line in body {
            lines.push(Line::styled(line.to_string(), Theme::dim()));
        }
    }

    if let Some(meta) = &commit.metadata
        && !meta.is_empty()
    {
        lines.push(Line::raw(""));
        lines.push(Line::styled("metadata", Theme::faint()));
        let mut keys: Vec<&String> = meta.keys().collect();
        keys.sort();
        for key in keys {
            lines.push(kv(key, &meta[key]));
        }
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner,
    );
}

fn kv(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<11}"), Theme::faint()),
        Span::styled(value.to_string(), Theme::file()),
    ])
}

/// The pane's own top padding sets these off from the border already.
fn note(text: &str) -> Paragraph<'static> {
    Paragraph::new(Line::styled(text.to_string(), Theme::faint())).centered()
}
