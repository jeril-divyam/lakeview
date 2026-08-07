//! Static help / keybinding reference.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap};

use super::truncate;
use crate::theme::Theme;

const NAVIGATION: &[(&str, &str)] = &[
    ("j / ↓", "move down"),
    ("k / ↑", "move up"),
    ("l / → / Enter", "open the selection"),
    ("h / ← / Bksp", "close the rightmost column"),
    ("g / G", "first / last entry"),
    ("Ctrl-d / Ctrl-u", "half-page down / up"),
];

const ACTIONS: &[(&str, &str)] = &[
    ("/", "filter the focused column"),
    ("Esc", "clear the filter / leave zoom"),
    ("y", "copy the lakefs:// URI"),
    ("r", "reload the focused column"),
    ("p", "switch profile"),
    ("1 / 2 / 3", "jump to a tab"),
    ("Tab", "cycle tabs"),
    ("q / Ctrl-c", "quit"),
];

const ABOUT: &[&str] = &[
    "Columns open to the right as you descend:",
    "repositories → refs → object prefixes.",
    "Older columns collapse when space runs out;",
    "‹ marks the ones scrolled off.",
    "Opening a file zooms its preview full-screen.",
    "A repo with one ref skips the branch column.",
    "",
    "Clicking an earlier column closes the ones to",
    "its right. The wheel moves the selection in the",
    "focused column, and only scrolls the view in the",
    "earlier ones. Set ui.mouse = false to hand the",
    "mouse back to your terminal.",
];

const MOUSE: &[(&str, &str)] = &[
    ("click", "select the row"),
    ("double-click", "open it"),
    ("right-click", "close the rightmost column"),
    ("wheel", "scroll under the cursor"),
];

const CONFIG: &str = "Profiles live in ~/.config/lakeview.toml. Each sets endpoint, access_key_id and \
secret_access_key, and may add default_repo, default_ref, verify_tls and timeout_secs. Credentials \
written as ${VAR} are read from the environment at start-up. Run `lakeview init` to write a starter \
file and `lakeview profiles` to list what is configured.";

pub fn draw(frame: &mut Frame, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    let [nav, mouse, about] = Layout::vertical([
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Min(3),
    ])
    .areas(left);
    let [actions, config] =
        Layout::vertical([Constraint::Length(10), Constraint::Min(4)]).areas(right);

    frame.render_widget(keys("Navigation", NAVIGATION, nav.width), nav);
    frame.render_widget(keys("Mouse", MOUSE, mouse.width), mouse);
    frame.render_widget(keys("Actions", ACTIONS, actions.width), actions);

    let lines: Vec<Line> = ABOUT
        .iter()
        .map(|l| Line::styled(l.to_string(), Theme::dim()))
        .collect();
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(panel("Layout")),
        about,
    );

    frame.render_widget(
        Paragraph::new(Text::styled(CONFIG, Theme::dim()))
            .wrap(Wrap { trim: false })
            .block(panel("Configuration")),
        config,
    );
}

/// A key/description table. Descriptions truncate rather than wrap, so the
/// chip column stays aligned.
fn keys(title: &str, rows: &[(&str, &str)], width: u16) -> Paragraph<'static> {
    let key_col = rows
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0)
        + 2;
    let room = (width as usize).saturating_sub(key_col + 4);

    let lines: Vec<Line> = rows
        .iter()
        .map(|(key, desc)| {
            let chip = format!(" {key} ");
            let pad = key_col + 2 - chip.chars().count();
            Line::from(vec![
                Span::styled(chip, Theme::chip()),
                Span::raw(" ".repeat(pad)),
                Span::styled(truncate(desc, room), Theme::dim()),
            ])
        })
        .collect();

    Paragraph::new(Text::from(lines)).block(panel(title))
}

fn panel(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Theme::border(false))
        .padding(Padding::horizontal(1))
        .title_top(Line::from(vec![
            Span::raw(" "),
            Span::styled(title.to_string(), Theme::title(false)),
            Span::raw(" "),
        ]))
}
