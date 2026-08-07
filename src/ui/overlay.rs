//! Centred popup overlays.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};

use super::truncate;
use crate::app::App;
use crate::theme::Theme;

pub fn profiles(frame: &mut Frame, app: &App, selected: usize, area: Rect) {
    let names = app.profile_names();
    // Borders, the padded row under the title, and slack — so the padding
    // doesn't cost the last profile its row.
    let height = (names.len() as u16 + 5)
        .min(area.height.saturating_sub(2))
        .max(5);
    let width = 54.min(area.width.saturating_sub(4));
    let popup = center(area, width, height);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Theme::border(true))
        .style(Theme::base())
        .padding(Padding::new(1, 1, 1, 0))
        .title_top(Line::from(vec![
            Span::raw(" "),
            Span::styled("Switch profile", Theme::accent_bold()),
            Span::raw(" "),
        ]));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    // Pad names to a common width so the endpoint column lines up.
    let name_col = names.iter().map(|n| n.chars().count()).max().unwrap_or(0);
    let room = (inner.width as usize).saturating_sub(name_col + 4);

    let lines: Vec<Line> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let endpoint = app
                .cfg
                .profiles
                .get(name)
                .map(|p| p.endpoint.trim_end_matches('/').to_string())
                .unwrap_or_default();
            let marker = if *name == app.profile_name {
                "● "
            } else {
                "  "
            };
            let label = format!("{name:name_col$}  ");
            let endpoint = truncate(&endpoint, room);
            let selected_row = i == selected;

            // Pad to the full width so the highlight covers the whole row.
            let used = marker.chars().count() + label.chars().count() + endpoint.chars().count();
            let tail = " ".repeat((inner.width as usize).saturating_sub(used));

            let (marker_style, name_style, endpoint_style) = if selected_row {
                (
                    Theme::selection(true).fg(Theme::ACCENT),
                    Theme::selection(true).fg(Theme::ACCENT),
                    Theme::selection(true).fg(Theme::DIM),
                )
            } else {
                (Theme::accent(), Theme::file(), Theme::faint())
            };

            let mut spans = vec![
                Span::styled(marker, marker_style),
                Span::styled(label, name_style),
                Span::styled(endpoint, endpoint_style),
            ];
            if selected_row {
                spans.push(Span::styled(tail, Theme::selection(true)));
            }
            Line::from(spans)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn center(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [cell] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(row);
    cell
}
