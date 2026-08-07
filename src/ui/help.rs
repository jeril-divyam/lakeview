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
    ("l / → / Enter", "expand / step right"),
    ("h / ← / Bksp", "collapse / step left"),
    ("space", "expand / collapse in place"),
    ("g / G", "first / last entry"),
    ("Ctrl-d / Ctrl-u", "half-page down / up"),
];

const ACTIONS: &[(&str, &str)] = &[
    ("/", "search the focused pane"),
    ("Esc", "clear the search / leave zoom"),
    ("y", "copy the lakefs:// URI"),
    ("r", "reload the focused pane"),
    ("p", "switch profile"),
    ("1 / 2 / 3", "jump to a tab"),
    ("Tab", "cycle tabs"),
    ("q / Ctrl-c", "quit"),
];

const ABOUT: &str = "Three panes: repositories, the object tree of one ref, and a \
detail/preview pane. Selecting a repository opens its default branch; → expands it to pick \
another branch or tag. In the tree → and ← open and close directories, → on a file zooms its \
preview, and at a pane's edge they move focus instead. / in the tree searches recursively, \
walking into closed directories and opening the path to every match; Esc restores the shape \
you had open. Set ui.mouse = false to hand the mouse back to your terminal.";

const MOUSE: &[(&str, &str)] = &[
    ("click", "focus the pane, select the row"),
    ("double-click", "expand / collapse, or open"),
    ("right-click", "collapse / go back"),
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
        Constraint::Length(9),
        Constraint::Length(6),
        Constraint::Min(3),
    ])
    .areas(left);
    let [actions, config] =
        Layout::vertical([Constraint::Length(10), Constraint::Min(4)]).areas(right);

    frame.render_widget(keys("Navigation", NAVIGATION, nav.width), nav);
    frame.render_widget(keys("Mouse", MOUSE, mouse.width), mouse);
    frame.render_widget(keys("Actions", ACTIONS, actions.width), actions);

    frame.render_widget(
        Paragraph::new(Text::styled(ABOUT, Theme::dim()))
            .wrap(Wrap { trim: false })
            .block(panel("Layout")),
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
