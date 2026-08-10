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
    ("l / →", "expand / step right"),
    ("Enter", "open a file full-screen"),
    ("h / ← / Bksp", "collapse / step left"),
    ("space", "expand / collapse in place"),
    ("g / G", "first / last entry"),
    ("Ctrl-d / Ctrl-u", "half-page down / up"),
];

const ACTIONS: &[(&str, &str)] = &[
    ("/", "search the focused pane"),
    ("Esc", "clear the search / leave zoom"),
    ("a / c", "in a zoom, unfold / fold all"),
    ("F", "filter the keys a .jsonl shows"),
    ("d", "download the selected file"),
    ("r", "reload the focused pane"),
    ("p", "switch profile"),
    ("1 / 2 / 3", "jump to a tab"),
    ("Tab", "cycle tabs"),
    ("q / Ctrl-c", "quit"),
];

const ABOUT: &str = "Three panes: repositories, the object tree of one ref, and a \
detail/preview pane. Selecting a repository opens its default branch; → expands it to pick \
another branch or tag. In the tree → and ← open and close directories, Enter on a file zooms its \
preview full-screen — Esc leaves it — where long lines wrap; the side pane lets them overflow. At a pane's edge → \
and ← move focus instead. / in the tree searches recursively, \
walking into closed directories and opening the path to every match; Esc restores the shape \
you had open. Set ui.mouse = false to hand the mouse back to your terminal.";

const FOLDING: &str = "A zoomed .json opens unfolded all the way down, the whole file on show. A \
zoomed .jsonl starts folded instead, listing its records a row apiece. \
→, Enter or space unfolds the selected row a level at a time; \
space folds it back up too, from its opening row or its closing bracket either way. ← winds back out, \
folding what is open and stepping out of what is not, and does nothing once nothing is left to \
close; Esc is what leaves the zoom. Folded rows are truncated — unfolding is how you see the rest \
— and everything else wraps. a on a folded row unfolds the whole document, all the way down; on \
an open one it brings every record to the level that row is open to. c folds all of it back up. A \
zoomed .json has nothing to level itself against, so a opens all of it and c shuts it to its own \
members. F over a zoomed .jsonl opens a menu of the keys its records use, \
each switchable: space switches one, ← → fold the tree, a/n switch all or none, Enter applies and \
Esc puts the old filter back.";

const MOUSE: &[(&str, &str)] = &[
    ("click", "focus the pane, select the row"),
    ("double-click", "expand / collapse, or open"),
    ("right-click", "collapse / go back"),
    ("wheel", "scroll under the cursor"),
    ("drag a border", "resize the panes either side"),
];

const CONFIG: &str = "Profiles live in ~/.config/lakeview.toml. Each sets endpoint, access_key_id and \
secret_access_key, and may add default_repo, default_ref, verify_tls and timeout_secs. Credentials \
written as ${VAR} are read from the environment at start-up. Run `lakeview init` to write a starter \
file and `lakeview profiles` to list what is configured.";

pub fn draw(frame: &mut Frame, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area);

    // Each table asks for its rows plus its borders and top padding. A table
    // given less than that quietly loses its last key, which is the one nobody
    // then knows about.
    let table_height = |rows: &[(&str, &str)]| Constraint::Length(rows.len() as u16 + 3);

    let [nav, mouse, about] = Layout::vertical([
        table_height(NAVIGATION),
        table_height(MOUSE),
        Constraint::Min(3),
    ])
    .areas(left);
    let [actions, folding, config] = Layout::vertical([
        table_height(ACTIONS),
        Constraint::Min(4),
        Constraint::Min(4),
    ])
    .areas(right);

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
        Paragraph::new(Text::styled(FOLDING, Theme::dim()))
            .wrap(Wrap { trim: false })
            .block(panel("Folding")),
        folding,
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
        .padding(Padding::new(1, 1, 1, 0))
        .title_top(Line::from(vec![
            Span::raw(" "),
            Span::styled(title.to_string(), Theme::title(false)),
            Span::raw(" "),
        ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key nobody can see is a key nobody knows about, so every row of every
    /// table has to survive the layout.
    #[test]
    fn every_key_is_drawn() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
        terminal.draw(|frame| draw(frame, frame.area())).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        for (key, _) in NAVIGATION.iter().chain(ACTIONS).chain(MOUSE) {
            assert!(screen.contains(key), "{key} is not on the help screen");
        }
    }
}
