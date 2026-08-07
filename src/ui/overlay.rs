//! Centred popup overlays.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::truncate;
use crate::app::App;
use crate::keys::MenuRow;
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

/// One level of nesting in the key menu.
const INDENT: &str = "  ";
/// Space kept between a menu line and either border, so the selection bar has
/// room to breathe without the block's padding eating into it.
const MARGIN: &str = " ";
/// Space kept clear of the zoom around the menu, as (columns, rows), so the
/// panel reads as sitting over the file rather than being part of it.
const GAP: (u16, u16) = (2, 1);

/// The key filter: the keys the zoomed records use, as a tree that unfolds one
/// level at a time the way the records themselves do, each switchable.
///
/// Sized to its content and centred over the zoom, so what it hides is visible
/// changing around it as the switches are thrown.
pub fn keys(frame: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.keys_rows();
    let lines: Vec<Line<'static>> = rows.iter().map(key_line).collect();

    let title = Span::styled("Filter keys", Theme::accent_bold());
    let footer = Line::from(vec![
        Span::styled(" space", Theme::accent()),
        Span::styled(" on/off  ", Theme::faint()),
        Span::styled("←→", Theme::accent()),
        Span::styled(" fold  ", Theme::faint()),
        Span::styled("a/n", Theme::accent()),
        Span::styled(" all/none  ", Theme::faint()),
        Span::styled("⏎", Theme::accent()),
        Span::styled(" apply  ", Theme::faint()),
        Span::styled("esc", Theme::accent()),
        Span::styled(" cancel ", Theme::faint()),
    ]);

    // Where the menu may sit: the zoom's own area, less the gap kept clear
    // around it. On a terminal too small for even that, the gap gives way.
    let room = Rect {
        x: area.x + GAP.0,
        y: area.y + GAP.1,
        width: area.width.saturating_sub(2 * GAP.0),
        height: area.height.saturating_sub(2 * GAP.1),
    };
    let room = if room.is_empty() { area } else { room };

    // Wide enough for the widest key and for both titles, so nothing the menu
    // needs in order to be worked is cut off. The lines carry their own side
    // margins, so only the borders are added here.
    let width = lines
        .iter()
        .map(|line| line.width() + 2)
        .chain([title.width() + 4, footer.width() + 2])
        .max()
        .unwrap_or(20)
        .min(room.width as usize) as u16;
    // Borders, plus a blank row above and below the keys.
    let height = ((lines.len() + 4) as u16).min(room.height);
    let popup = Rect {
        x: room.x + (room.width.saturating_sub(width)) / 2,
        y: room.y + (room.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Theme::border(true))
        .style(Theme::base())
        .padding(Padding::vertical(1))
        .title_top(Line::from(vec![Span::raw(" "), title, Span::raw(" ")]))
        .title_bottom(footer);
    let list = block.inner(popup);

    // Clear the gap along with the menu, so the file behind does not run right
    // up against the border. The rows above and below are cleared right across
    // the pane rather than to the panel's own width: half a row of a record,
    // or a line number with nothing after it, reads as a rendering fault.
    for band in [
        Rect::new(area.x, popup.y.saturating_sub(GAP.1), area.width, GAP.1),
        Rect::new(area.x, popup.bottom(), area.width, GAP.1),
        grow(popup, (GAP.0, 0)),
    ] {
        frame.render_widget(Clear, band.intersection(area));
    }
    frame.render_widget(block, popup);

    app.keys.popup = popup;
    app.keys.list = list;
    app.keys_resize(list.height as usize);

    for y in 0..list.height {
        let idx = app.keys.scroll + y as usize;
        let Some(line) = lines.get(idx) else {
            break;
        };
        let mut line = line.clone();
        if idx == app.keys.cursor {
            line = line.style(Theme::selection(true));
        }
        line.render(
            Rect::new(list.x, list.y + y, list.width, 1),
            frame.buffer_mut(),
        );
    }

    // A key tree taller than the menu has to say so, or a filter can look like
    // it has fewer keys than it does. How many are switched off is the pane
    // title's job.
    if lines.len() > list.height as usize {
        let more = format!(
            " {}/{} ",
            (app.keys.cursor + 1).min(lines.len()),
            lines.len()
        );
        let x = popup.x + popup.width.saturating_sub(more.width() as u16 + 1);
        Line::from(Span::styled(more, Theme::faint())).render(
            Rect::new(x, popup.y, popup.right() - x, 1),
            frame.buffer_mut(),
        );
    }
}

/// One menu line: indent, fold marker, switch, key.
fn key_line(row: &MenuRow) -> Line<'static> {
    let marker = match (row.has_children, row.open) {
        (true, true) => "▾ ",
        (true, false) => "▸ ",
        (false, _) => "  ",
    };
    // `[~]` is on, but hiding something further down.
    let (switch, switch_style) = match (row.enabled, row.partial) {
        (false, _) => ("[ ] ", Theme::faint()),
        (true, true) => ("[~] ", Theme::accent()),
        (true, false) => ("[x] ", Theme::ok()),
    };
    let key_style = if row.enabled {
        Theme::file()
    } else {
        Theme::faint().add_modifier(Modifier::CROSSED_OUT)
    };
    // The side margins live in the line rather than in the block's padding, so
    // the selected row's highlight still reaches both borders.
    Line::from(vec![
        Span::raw(format!("{MARGIN}{}", INDENT.repeat(row.depth()))),
        Span::styled(marker, Theme::accent()),
        Span::styled(switch, switch_style),
        Span::styled(row.key.clone(), key_style),
        Span::raw(MARGIN),
    ])
}

/// `rect` grown by `(columns, rows)` on every side.
fn grow(rect: Rect, by: (u16, u16)) -> Rect {
    Rect {
        x: rect.x.saturating_sub(by.0),
        y: rect.y.saturating_sub(by.1),
        width: rect.width + 2 * by.0,
        height: rect.height + 2 * by.1,
    }
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
