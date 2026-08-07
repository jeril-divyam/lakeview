//! The browser: repositories, a tree of one ref's objects, and a
//! detail/preview pane, in three fixed columns.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, HighlightSpacing, List, ListItem, Padding, Paragraph, Wrap,
};

use super::{format_ts, human_size, justify, truncate};
use crate::app::{App, Focus, Load, Mode, PreviewBody, ReposRow, ReposView, RowKind, TreeView};
use crate::theme::Theme;

/// The tree pane never shrinks below this; the preview goes first instead.
const MIN_TREE: u16 = 24;
/// Room a tree row keeps for its name however deep it sits.
const MIN_NAME: usize = 10;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    app.hits.repos = None;
    app.hits.tree = None;
    app.hits.preview = None;
    app.hits.commits = None;

    if app.mode == Mode::Zoom {
        app.hits.preview = Some(draw_detail(frame, app, area, true));
        return;
    }

    // The repository pane takes its fixed width; the tree and the preview
    // split what is left evenly. On a narrow terminal panes drop out rather
    // than being crushed — the preview goes first, then the repository list,
    // leaving the focused pane the whole width.
    let repos_w = app.cfg.ui.repos_width.clamp(14, 40);
    let show_repos = area.width >= repos_w + MIN_TREE;
    let remainder = area.width.saturating_sub(if show_repos { repos_w } else { 0 });
    let show_preview = app.cfg.ui.preview_percent > 0 && remainder >= 2 * MIN_TREE;

    let mut constraints = Vec::with_capacity(3);
    if show_repos {
        constraints.push(Constraint::Length(repos_w));
    }
    constraints.push(Constraint::Fill(1));
    if show_preview {
        constraints.push(Constraint::Fill(1));
    }
    let chunks = Layout::horizontal(constraints).split(area);

    let mut next = 0;
    if show_repos {
        let slot = chunks[next];
        next += 1;
        app.hits.repos = Some(draw_repos(
            frame,
            &mut app.repos,
            slot,
            app.focus == Focus::Repos,
            app.tick,
        ));
    }

    let slot = chunks[next];
    next += 1;
    app.hits.tree = Some(draw_tree(
        frame,
        &mut app.tree,
        slot,
        app.focus == Focus::Tree,
        app.tick,
    ));

    if show_preview {
        app.hits.preview = Some(draw_detail(frame, app, chunks[next], false));
    }
}

/// A pane frame with a left title and a right-aligned annotation.
fn pane_block<'a>(title: &str, right: Vec<Span<'a>>, focused: bool, width: u16) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Theme::border(focused))
        .title_top(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                truncate(title, width.saturating_sub(8) as usize),
                Theme::title(focused),
            ),
            Span::raw(" "),
        ]))
        .title_top(Line::from(right).right_aligned())
}

fn annotation<'a>(filter: &str, count: usize, ready: bool) -> Vec<Span<'a>> {
    let mut right = Vec::new();
    if !filter.is_empty() {
        right.push(Span::styled(
            format!(" /{} ", truncate(filter, 8)),
            Theme::accent(),
        ));
    }
    if ready && count > 0 {
        right.push(Span::styled(format!(" {count} "), Theme::faint()));
    }
    right
}

fn centered_note(text: &str, style: Style) -> Paragraph<'static> {
    Paragraph::new(Line::styled(text.to_string(), style))
        .centered()
        .block(Block::default().padding(Padding::top(1)))
}

fn spinner(tick: usize) -> &'static str {
    super::SPINNER[(tick / 2) % super::SPINNER.len()]
}

// ── pane 1: repositories ─────────────────────────────────────────────────

fn draw_repos(
    frame: &mut Frame,
    repos: &mut ReposView,
    area: Rect,
    focused: bool,
    tick: usize,
) -> Rect {
    let right = annotation(&repos.filter, repos.rows.len(), true);
    let block = pane_block("Repositories", right, focused, area.width);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if repos.rows.is_empty() {
        let note = if !repos.filter.is_empty() {
            "no matches"
        } else if repos.repos.is_empty() {
            "no repositories"
        } else {
            "empty"
        };
        frame.render_widget(centered_note(note, Theme::faint()), inner);
        return inner;
    }

    // Two columns go to the highlight bar and its trailing space.
    let width = inner.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = repos
        .rows
        .iter()
        .map(|row| ListItem::new(render_repo_row(row, width, tick)))
        .collect();

    let list = List::new(items)
        .highlight_style(Theme::selection(focused))
        .highlight_symbol(if focused { "▌" } else { " " })
        .highlight_spacing(HighlightSpacing::Always)
        .scroll_padding(2);
    frame.render_stateful_widget(list, inner, &mut repos.state);
    inner
}

fn render_repo_row(row: &ReposRow, width: usize, tick: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let style = match row.kind {
        RowKind::Repo => Style::new().fg(Theme::ACCENT),
        RowKind::Branch if row.primary => Style::new()
            .fg(Theme::GREEN)
            .add_modifier(Modifier::BOLD),
        RowKind::Branch => Style::new().fg(Theme::FG),
        RowKind::Tag => Style::new().fg(Theme::PURPLE),
    };

    // A repository's chevron doubles as its icon; refs indent beneath it.
    let prefix = match row.reference {
        None if row.loading => format!("{} ", spinner(tick)),
        None if row.expanded => "▾ ".into(),
        None => "▸ ".into(),
        Some(_) => {
            let icon = match row.kind {
                RowKind::Branch if row.primary => "● ",
                RowKind::Branch => "○ ",
                RowKind::Tag => "◇ ",
                _ => "  ",
            };
            format!("  {icon}")
        }
    };
    let used = prefix.chars().count();
    spans.push(Span::styled(prefix, style));

    let (label, pad) = justify(&row.label, &row.meta, width.saturating_sub(used));
    spans.push(Span::styled(label, style));
    spans.push(Span::raw(pad));
    spans.push(Span::styled(row.meta.clone(), Theme::faint()));
    Line::from(spans)
}

// ── pane 2: the object tree ──────────────────────────────────────────────

fn draw_tree(
    frame: &mut Frame,
    tree: &mut TreeView,
    area: Rect,
    focused: bool,
    tick: usize,
) -> Rect {
    let title = match &tree.key {
        Some((_, reference)) => reference.clone(),
        None => "Tree".to_string(),
    };
    let mut right = annotation(
        &tree.filter,
        tree.rows.len(),
        matches!(tree.root_load(), Load::Ready),
    );
    if tree.capped {
        right.insert(0, Span::styled(" capped ", Theme::error()));
    }
    let block = pane_block(&title, right, focused, area.width);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if tree.key.is_none() {
        frame.render_widget(
            centered_note("select a repository", Theme::faint()),
            inner,
        );
        return inner;
    }
    match tree.root_load() {
        Load::Idle | Load::Loading => {
            frame.render_widget(
                centered_note(&format!("{} loading…", spinner(tick)), Theme::dim()),
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

    if tree.rows.is_empty() {
        let note = if tree.filter.is_empty() {
            "empty"
        } else if tree.crawling {
            "searching…"
        } else {
            "no matches"
        };
        frame.render_widget(centered_note(note, Theme::faint()), inner);
        return inner;
    }

    let width = inner.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = tree
        .rows
        .iter()
        .map(|slot| ListItem::new(render_tree_row(tree, *slot, width, tick)))
        .collect();

    let list = List::new(items)
        .highlight_style(Theme::selection(focused))
        .highlight_symbol(if focused { "▌" } else { " " })
        .highlight_spacing(HighlightSpacing::Always)
        .scroll_padding(2);
    frame.render_stateful_widget(list, inner, &mut tree.state);
    inner
}

fn render_tree_row(tree: &TreeView, slot: usize, width: usize, tick: usize) -> Line<'static> {
    let node = &tree.nodes[slot];

    // Deep nesting gives ground before the name does.
    let indent = (node.depth * 2).min(width.saturating_sub(MIN_NAME));
    let (marker, marker_style) = if node.is_dir() {
        match node.load {
            Load::Loading => (format!("{} ", spinner(tick)), Theme::dim()),
            Load::Failed(_) => ("! ".to_string(), Theme::error()),
            _ if tree.is_open(slot) => ("▾ ".to_string(), Theme::directory()),
            _ => ("▸ ".to_string(), Theme::directory()),
        }
    } else {
        ("  ".to_string(), Theme::file())
    };

    let style = if node.is_dir() {
        Theme::directory()
    } else {
        Theme::file()
    };
    let label = if node.is_dir() {
        format!("{}/", node.name)
    } else {
        node.name.clone()
    };
    let meta = if node.is_dir() {
        String::new()
    } else {
        human_size(node.stat.size_bytes.unwrap_or(0))
    };

    let body = width.saturating_sub(indent + marker.chars().count());
    let (label, pad) = justify(&label, &meta, body);
    Line::from(vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(marker, marker_style),
        Span::styled(label, style),
        Span::raw(pad),
        Span::styled(meta, Theme::faint()),
    ])
}

// ── pane 3: detail / preview ─────────────────────────────────────────────

/// Renders the detail/preview pane and returns its inner area.
fn draw_detail(frame: &mut Frame, app: &App, area: Rect, zoomed: bool) -> Rect {
    let heading = match app.focus {
        Focus::Repos => app.repos.selected_row().map(|r| r.label.clone()),
        Focus::Tree => app.tree.selected().map(|n| n.name.clone()),
    }
    .unwrap_or_else(|| "Details".into());

    let mut right = Vec::new();
    if zoomed {
        right.push(Span::styled(" zoom ", Theme::chip()));
        right.push(Span::raw(" "));
    }
    let block = pane_block(&heading, right, zoomed, area.width)
        .padding(Padding::horizontal(1));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = match app.focus {
        Focus::Repos => repos_detail(app),
        Focus::Tree => tree_detail(app, inner.width),
    };

    let paragraph = Paragraph::new(Text::from(lines)).scroll((app.preview.scroll, 0));
    frame.render_widget(paragraph, inner);
    inner
}

fn repos_detail(app: &App) -> Vec<Line<'static>> {
    let Some(row) = app.repos.selected_row() else {
        return vec![Line::styled("nothing selected", Theme::faint())];
    };
    let mut lines = Vec::new();

    match &row.reference {
        None => {
            let Some(repo) = app.repos.repos.iter().find(|r| r.id == row.repo) else {
                return lines;
            };
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
            lines.push(Line::raw(""));
            // The pane is narrow and does not wrap, so hints stay short.
            lines.push(Line::styled("→  branches and tags", Theme::faint()));
            lines.push(Line::styled("2  commit log", Theme::faint()));
        }
        Some(reference) => {
            let found = app
                .repos
                .refs
                .get(&row.repo)
                .and_then(|slot| slot.refs.iter().find(|r| r.id == *reference));
            let Some(r) = found else { return lines };
            let kind = match r.kind {
                crate::lakefs::RefKind::Branch => "branch",
                crate::lakefs::RefKind::Tag => "tag",
            };
            lines.push(kv("name", &r.id, Theme::accent()));
            lines.push(kv("repository", &row.repo, Theme::file()));
            lines.push(kv("type", kind, Theme::file()));
            if r.is_default {
                lines.push(kv("default", "yes", Style::new().fg(Theme::GREEN)));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled("head commit", Theme::faint()));
            lines.push(Line::styled(r.commit_id.clone(), Theme::dim()));
            lines.push(Line::raw(""));
            lines.push(Line::styled("→  browse this ref", Theme::faint()));
            lines.push(Line::styled("2  commit log", Theme::faint()));
        }
    }
    lines
}

fn tree_detail(app: &App, width: u16) -> Vec<Line<'static>> {
    let Some(node) = app.tree.selected() else {
        return vec![Line::styled("nothing selected", Theme::faint())];
    };
    let mut lines = Vec::new();

    if node.is_dir() {
        lines.push(kv("prefix", &node.name, Theme::directory()));
        let counted = node.children.len();
        if matches!(node.load, Load::Ready) {
            lines.push(kv("entries", &counted.to_string(), Theme::file()));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(node.stat.path.clone(), Theme::dim()));
        lines.push(Line::raw(""));
        lines.push(Line::styled("→  open      ←  close", Theme::faint()));
        lines.push(Line::styled("space  toggle", Theme::faint()));
        return lines;
    }

    lines.extend(object_details(&node.stat));
    lines.push(Line::raw(""));
    lines.extend(preview_lines(app, width));
    lines
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
    let mut lines = vec![Line::styled("─".repeat(width as usize), Theme::faint())];

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
