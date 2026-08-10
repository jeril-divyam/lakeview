//! The browser: repositories, a tree of one ref's objects, and a
//! detail/preview pane, in three fixed columns.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, HighlightSpacing, List, ListItem, Padding, Paragraph, Wrap,
};

use unicode_width::UnicodeWidthStr;

use super::{format_ts, human_size, justify, truncate};
use crate::app::{App, Focus, Load, PreviewBody, ReposRow, ReposView, RowKind, TreeView};
use crate::jsonl::{DocRow, Row as JsonRow};
use crate::theme::Theme;

/// Floors the configured widths are held to, so no ratio can squeeze a pane
/// down to nothing. The preview drops out before the tree reaches its floor.
const MIN_REPOS: u16 = 12;
const MIN_TREE: u16 = 24;
const MIN_PREVIEW: u16 = 20;
/// Room a tree row keeps for its name however deep it sits.
const MIN_NAME: usize = 10;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    app.hits.repos = None;
    app.hits.tree = None;
    app.hits.preview = None;
    app.hits.preview_rows.clear();
    app.hits.commits = None;

    // The key menu is a panel over the zoom rather than a place of its own, so
    // the zoom is still what is drawn under it.
    if app.zoomed() {
        app.hits.preview = Some(draw_detail(frame, app, area, true));
        return;
    }

    // The repositories pane takes its configured columns; the tree and the
    // preview divide what is left by their ratios. On a narrow terminal panes
    // drop out rather than being crushed — the preview goes first, then the
    // repository list, leaving the focused pane the whole width.
    let ui = &app.cfg.ui;
    let show_repos = area.width >= MIN_REPOS + MIN_TREE;
    let repos_w = ui
        .repos_width
        .max(MIN_REPOS)
        .min(area.width.saturating_sub(MIN_TREE));
    let remainder = area.width.saturating_sub(if show_repos { repos_w } else { 0 });

    let tree_ratio = ui.tree_ratio.max(1) as u32;
    let preview_ratio = ui.preview_ratio as u32;
    let show_preview = preview_ratio > 0 && remainder >= MIN_TREE + MIN_PREVIEW;

    let mut constraints = Vec::with_capacity(3);
    if show_repos {
        constraints.push(Constraint::Length(repos_w));
    }
    if show_preview {
        // Honour the ratio, but never past either pane's floor. The preview
        // takes the remaining columns, so rounding can't leave a gap.
        let tree_w = (remainder as u32 * tree_ratio / (tree_ratio + preview_ratio)) as u16;
        constraints.push(Constraint::Length(
            tree_w.clamp(MIN_TREE, remainder - MIN_PREVIEW),
        ));
    }
    constraints.push(Constraint::Fill(1));
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

/// A pane frame with a left title and a right-aligned annotation. Every pane
/// keeps a row of breathing room under its title; panes that want side padding
/// too restate the whole set, since `padding` replaces rather than merges.
///
/// The list panes pass a grey `title_style` whether or not they have focus — the
/// border and the selection bar already say where you are, so a pane's name and
/// the ref it is showing are context rather than the thing to look at.
fn pane_block<'a>(
    title: &str,
    title_style: Style,
    right: Vec<Span<'a>>,
    focused: bool,
    width: u16,
) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Theme::border(focused))
        .padding(Padding::top(1))
        .title_top(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                truncate(title, width.saturating_sub(8) as usize),
                title_style,
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

/// The pane's own top padding sets these off from the border already.
fn centered_note(text: &str, style: Style) -> Paragraph<'static> {
    Paragraph::new(Line::styled(text.to_string(), style)).centered()
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
    let block = pane_block(
        "Repositories",
        Theme::title(false),
        right,
        focused,
        area.width,
    );
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

    // A repository's chevron doubles as its icon; refs indent beneath it. One
    // with nothing to list takes the same `●` its lone default branch would have
    // worn a row below — the row stands for that branch, so it wears its mark.
    let prefix = match row.reference {
        None if row.loading => format!("{} ", spinner(tick)),
        None if row.expanded => "▾ ".into(),
        None if !row.expandable => "● ".into(),
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
    let block = pane_block(&title, Theme::title(false), right, focused, area.width);
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
fn draw_detail(frame: &mut Frame, app: &mut App, area: Rect, zoomed: bool) -> Rect {
    let heading = match app.focus {
        Focus::Repos => app.repos.selected_row().map(|r| r.label.clone()),
        Focus::Tree => app.tree.selected().map(|n| n.name.clone()),
    }
    .unwrap_or_else(|| "Details".into());

    // Records dropped with the fetch are worth saying out loud: unlike a cut
    // line of text, a missing record leaves nothing behind to notice.
    let mut right = Vec::new();
    if app.jsonl().is_some_and(|doc| doc.truncated) {
        right.push(Span::styled(" truncated ", Theme::faint()));
    }
    // What the key filter is holding back, for as long as it holds it back — a
    // key that is simply absent from every record looks the same otherwise.
    if let Some(hidden) = app
        .jsonl()
        .map(|doc| doc.keys().hidden())
        .filter(|n| *n > 0)
    {
        right.push(Span::styled(
            format!(" {hidden} key{} hidden ", if hidden == 1 { "" } else { "s" }),
            Theme::accent(),
        ));
    }
    if zoomed {
        right.push(Span::styled(" zoom ", Theme::chip()));
        right.push(Span::raw(" "));
    }
    let block = pane_block(&heading, Theme::title(zoomed), right, zoomed, area.width)
        .padding(Padding::new(1, 1, 1, 0));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // A zoomed foldable document is a list of its own, with a selection and rows
    // to unfold, so it draws itself rather than flattening to a paragraph.
    if zoomed && app.zoom_doc().is_some() {
        draw_zoom(frame, app, inner);
        return inner;
    }

    let mut lines = match app.focus {
        Focus::Repos => repos_detail(app),
        Focus::Tree => tree_detail(app, inner.width),
    };

    // Only zoom re-flows. In the side pane a long JSON string would cost a
    // dozen lines to show whole, burying the rest of the file; at full width
    // wrapping is what you want, since nothing else is competing for the room.
    if zoomed {
        lines = lines
            .into_iter()
            .flat_map(|line| {
                let indent = hanging_indent(&line);
                wrap_line(line, inner.width as usize, indent)
            })
            .collect();
    }

    // Scrolling is clamped here because this is the only place the wrapped
    // height is known — which is also what lets `G` ask for the bottom.
    let scroll = clamp_scroll(app.preview.scroll, lines.len(), inner.height);
    app.preview.scroll = scroll;
    let paragraph = Paragraph::new(Text::from(lines)).scroll((scroll, 0));
    frame.render_widget(paragraph, inner);
    inner
}

/// Hold a scroll offset to a body of `lines` rendered `height` rows tall.
fn clamp_scroll(scroll: u16, lines: usize, height: u16) -> u16 {
    let max = lines.saturating_sub(height as usize);
    scroll.min(max.min(u16::MAX as usize) as u16)
}

// ── the zoomed foldable view ─────────────────────────────────────────────

/// A folded document, one row per record or per JSON member, each unfolding a
/// level at a time.
///
/// A row showing a folded value is truncated rather than wrapped — unfolding it
/// is how you see the rest, and wrapping one row over ten lines would bury the
/// rows under it. Everything else wraps, so the long string values an unfolded
/// container exposes can be read at full width.
fn draw_zoom(frame: &mut Frame, app: &mut App, area: Rect) {
    let (width, height) = (area.width as usize, area.height as usize);
    if width == 0 || height == 0 {
        return;
    }

    // Take everything the layout needs by value, so the doc isn't still
    // borrowed when the scroll and the hit map are written back.
    let Some((rows, cursor)) = zoom_rows(app) else {
        return;
    };
    if rows.is_empty() {
        frame.render_widget(centered_note("nothing to show", Theme::faint()), area);
        return;
    }
    let lines = zoom_layout(rows, cursor, width);
    let first = lines.iter().position(|(r, _)| *r == cursor).unwrap_or(0);
    let last = lines
        .iter()
        .rposition(|(r, _)| *r == cursor)
        .map_or(first + 1, |i| i + 1);

    let top = reveal(app.preview.scroll as usize, first, last, height, lines.len());
    app.preview.scroll = top.min(u16::MAX as usize) as u16;

    app.hits.preview_row_starts = row_starts(&lines);
    app.hits.preview_lines = lines.len();
    let visible = lines.iter().skip(top).take(height);
    app.hits.preview_rows = visible.clone().map(|(row, _)| *row).collect();
    for (y, (_, line)) in visible.enumerate() {
        let slot = Rect::new(area.x, area.y + y as u16, area.width, 1);
        frame.render_widget(line, slot);
    }
}

/// The styled rows of whichever foldable document the zoom has, each paired with
/// whether it is folded, plus the cursor held inside the document.
///
/// The two kinds differ only in their gutter: a JSONL row is numbered by the
/// record it belongs to, a JSON row by its own place in the document.
fn zoom_rows(app: &App) -> Option<(Vec<(Line<'static>, bool)>, usize)> {
    match &app.preview.body {
        Some(PreviewBody::Jsonl(doc)) => {
            let rows = doc.rows();
            let cursor = doc.cursor.min(rows.len().saturating_sub(1));
            let gutter = doc.entries.len().to_string().len().max(2);
            let lit = rows.get(cursor).map(|r| r.entry);
            let lines = rows
                .iter()
                .map(|r| (jsonl_line(r, gutter, Some(r.entry) == lit), r.folded))
                .collect();
            Some((lines, cursor))
        }
        Some(PreviewBody::Json { doc, .. }) => {
            let rows = doc.rows();
            let cursor = doc.cursor.min(rows.len().saturating_sub(1));
            let gutter = rows.len().to_string().len().max(2);
            let lines = rows
                .iter()
                .enumerate()
                .map(|(i, r)| (json_line(r, i + 1, gutter), r.folded))
                .collect();
            Some((lines, cursor))
        }
        _ => None,
    }
}

/// Lay every row out into the screen lines it occupies, each tagged with the row
/// it came from. Everything is laid out, not just what is on screen: the cursor
/// cannot be revealed, nor the view held to the end of the body, without knowing
/// how tall the rows above it are.
fn zoom_layout(
    rows: Vec<(Line<'static>, bool)>,
    cursor: usize,
    width: usize,
) -> Vec<(usize, Line<'static>)> {
    let mut out = Vec::with_capacity(rows.len());
    for (i, (line, folded)) in rows.into_iter().enumerate() {
        let laid_out = if folded {
            vec![truncate_line(line, width)]
        } else {
            let indent = hanging_indent(&line);
            wrap_line(line, width, indent)
        };
        for line in laid_out {
            // The whole of a wrapped row lights up, so the selection reads as
            // one row rather than as the one line the cursor happens to be on.
            let line = if i == cursor {
                line.style(Theme::selection(true))
            } else {
                line
            };
            out.push((i, line));
        }
    }
    out
}

/// The screen line each row begins at. Rows are laid out in order, so the first
/// line naming a row is the line it starts at, and one pass records them all.
/// This is what paging measures a screenful against.
fn row_starts(lines: &[(usize, Line<'static>)]) -> Vec<usize> {
    let mut starts = Vec::new();
    for (line, (row, _)) in lines.iter().enumerate() {
        if *row == starts.len() {
            starts.push(line);
        }
    }
    starts
}

/// Where the view should start so that the selected row — lines `first..last` —
/// is on screen, moving as little as it can.
fn reveal(top: usize, first: usize, last: usize, height: usize, total: usize) -> usize {
    let top = if first < top {
        // Scrolled past the selection: bring it back to the top edge.
        first
    } else if last > top + height {
        // Below the fold: show as much of the row as fits, without ever pushing
        // its first line off the top — one taller than the screen reads from
        // its start.
        if last - first <= height {
            last - height
        } else {
            first
        }
    } else {
        top
    };
    // Never leave blank rows under the end of the body.
    top.min(total.saturating_sub(height))
}

/// A record's row: the gutter, then the row's own coloured cells. The record
/// number lights up for every row of the record the cursor is inside, and the
/// body of an expanded record hangs off it under a rule.
fn jsonl_line(row: &DocRow, gutter: usize, lit: bool) -> Line<'static> {
    let mut spans = Vec::with_capacity(row.cells.len() + 1);
    if row.sub == 0 {
        let style = if lit {
            Theme::accent_bold()
        } else {
            Theme::faint()
        };
        spans.push(Span::styled(
            format!("{:>gutter$} ", row.entry + 1),
            style,
        ));
    } else {
        spans.push(Span::styled(
            format!("{:>gutter$} │ ", ""),
            Theme::faint(),
        ));
    }
    spans.extend(
        row.cells
            .iter()
            .map(|(tok, text)| Span::styled(text.clone(), Theme::json(*tok))),
    );
    Line::from(spans)
}

/// A row of a whole JSON file: its line number, then the row's coloured cells.
///
/// The number counts rows on show rather than lines of the file, so folding a
/// block renumbers what is under it — there is no original line to keep, the
/// document being re-indented from the parsed value in the first place.
fn json_line(row: &JsonRow, number: usize, gutter: usize) -> Line<'static> {
    let mut spans = Vec::with_capacity(row.cells.len() + 1);
    spans.push(Span::styled(format!("{number:>gutter$} "), Theme::faint()));
    spans.extend(
        row.cells
            .iter()
            .map(|(tok, text)| Span::styled(text.clone(), Theme::json(*tok))),
    );
    Line::from(spans)
}

/// Cut a line to `width` display columns, marking the cut with an ellipsis.
/// Unlike `wrap_line` this keeps the row one row tall, whatever it holds.
fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    let total: usize = line.spans.iter().map(|s| s.content.width()).sum();
    if total <= width {
        return line;
    }
    if width == 0 {
        return Line::default();
    }
    let style = line.style;
    let room = width - 1;

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in line.spans {
        if used == room {
            break;
        }
        let span_width = span.content.width();
        if used + span_width <= room {
            used += span_width;
            spans.push(span);
            continue;
        }
        // The cut lands inside this span; keep whichever characters still fit.
        let mut kept = String::new();
        for ch in span.content.chars() {
            let w = char_width(ch);
            if used + w > room {
                break;
            }
            used += w;
            kept.push(ch);
        }
        if !kept.is_empty() {
            spans.push(Span::styled(kept, span.style));
        }
        break;
    }
    spans.push(Span::styled("…", Theme::faint()));
    Line::from(spans).style(style)
}

/// Where a wrapped continuation should start: under the content, past the
/// gutter or label prefixing the line, plus any indentation the content itself
/// carries. A line that is a single span has no such prefix — a hex dump row,
/// a bare path — so its continuations start at the margin.
fn hanging_indent(line: &Line) -> usize {
    if line.spans.len() < 2 {
        return 0;
    }
    let prefix = line.spans[0].content.width();
    let leading = line.spans[1..]
        .iter()
        .flat_map(|s| s.content.chars())
        .take_while(|c| *c == ' ')
        .count();
    prefix + leading
}

/// Re-flow one line to `width`, indenting every line after the first by
/// `indent`. Styles survive the break, so a wrapped JSON string keeps its
/// colour. Words are kept whole where they fit; anything longer than the line
/// on its own — a base64 blob, a long URL — is broken hard, since refusing to
/// break it would just push it off the edge again.
fn wrap_line(line: Line<'static>, width: usize, indent: usize) -> Vec<Line<'static>> {
    let total: usize = line.spans.iter().map(|s| s.content.width()).sum();
    if width < 8 || total <= width {
        return vec![line];
    }
    // Never let the indent crowd out the text it is meant to align.
    let indent = indent.min(width / 2);

    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| {
            let style = span.style;
            span.content.chars().map(move |c| (c, style))
        })
        .collect();

    let mut out = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        let first = out.is_empty();
        let room = if first { width } else { width - indent };

        // Take as much as fits.
        let mut end = start;
        let mut used = 0;
        while end < cells.len() {
            let w = char_width(cells[end].0);
            if used + w > room {
                break;
            }
            used += w;
            end += 1;
        }
        if end == cells.len() {
            out.push(cells_to_line(&cells[start..end], if first { 0 } else { indent }));
            break;
        }

        // Back off to the last space so words stay whole — but not so far back
        // that most of the line goes to waste.
        let mut brk = end;
        if let Some(pos) = cells[start..end].iter().rposition(|(c, _)| *c == ' ')
            && start + pos + 1 > start + room / 2
        {
            brk = start + pos + 1;
        }
        out.push(cells_to_line(&cells[start..brk], if first { 0 } else { indent }));

        start = brk;
        // A continuation shouldn't open with the space it broke on.
        while start < cells.len() && cells[start].0 == ' ' {
            start += 1;
        }
    }
    // The cells carry only what their own spans set, so a line styled as a whole
    // — `Line::styled(text, style)`, one bare span under a line-level style —
    // would come back out of here uncoloured. Hand that style to every piece.
    let style = line.style;
    out.into_iter().map(|line| line.style(style)).collect()
}

fn char_width(c: char) -> usize {
    UnicodeWidthStr::width(c.to_string().as_str())
}

/// Rebuild a line from styled cells, merging runs that share a style back into
/// single spans so the frame isn't one span per character.
fn cells_to_line(cells: &[(char, Style)], indent: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if indent > 0 {
        spans.push(Span::raw(" ".repeat(indent)));
    }
    for (c, style) in cells {
        match spans.last_mut() {
            Some(prev) if prev.style == *style => prev.content.to_mut().push(*c),
            _ => spans.push(Span::styled(c.to_string(), *style)),
        }
    }
    Line::from(spans)
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

    // A file is its contents. What lakeFS knows about the object besides them —
    // its size, its type, the commit behind it, where it sits in the store — is
    // either on its row in the tree already or not what anyone opened the pane
    // to read, so the preview has the whole of it.
    lines.extend(preview_lines(app, width));
    lines
}

fn preview_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    // No rule at the top: it used to divide the object's details from its
    // contents, and there are no details left above it to divide them from.
    let mut lines: Vec<Line<'static>> = Vec::new();

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
        Some(PreviewBody::Json { lines: rows, .. }) => {
            let gutter = rows.len().to_string().len().max(2);
            for (i, row) in rows.iter().enumerate() {
                let mut spans = vec![Span::styled(format!("{:>gutter$} ", i + 1), Theme::faint())];
                spans.extend(
                    row.iter()
                        .map(|(tok, text)| Span::styled(text.clone(), Theme::json(*tok))),
                );
                // A file is one value read top to bottom, so a long string in the
                // middle of it is worth the lines it takes: cutting it at the
                // edge hides the only part that says what the value is. The wrap
                // hangs under the nesting, so the shape still reads down the pane
                // and the numbered lines stay the file's own.
                let line = Line::from(spans);
                let indent = hanging_indent(&line);
                lines.extend(wrap_line(line, width as usize, indent));
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
        // Records are only worth folding at full width, so the side pane gives
        // each one a line, coloured like the JSON preview beside it but never
        // folded; `→` zooms to the view that unfolds them.
        Some(PreviewBody::Jsonl(doc)) => {
            let gutter = doc.entries.len().to_string().len().max(2);
            for (i, entry) in doc.entries.iter().enumerate() {
                let mut spans = vec![Span::styled(format!("{:>gutter$} ", i + 1), Theme::faint())];
                spans.extend(
                    entry
                        .line()
                        .iter()
                        .map(|(tok, text)| Span::styled(text.clone(), Theme::json(*tok))),
                );
                lines.push(Line::from(spans));
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

#[cfg(test)]
mod tests {
    use super::*;
    // The browser draws from `App::zoomed`, so only the tests, which drive the
    // modes directly, still name them.
    use crate::app::Mode;
    use crate::jsonl::Folding;

    /// Flatten a rendered line back to plain text, for comparison.
    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn wrapped(line: Line<'static>, width: usize) -> Vec<String> {
        let indent = hanging_indent(&line);
        wrap_line(line, width, indent).iter().map(text).collect()
    }

    #[test]
    fn short_lines_are_left_alone() {
        let line = Line::from(vec![Span::raw(" 1 "), Span::raw("hello")]);
        assert_eq!(wrapped(line, 40), vec![" 1 hello"]);
    }

    #[test]
    fn continuations_hang_under_the_gutter() {
        let line = Line::from(vec![Span::raw(" 1 "), Span::raw("alpha beta gamma delta")]);
        let out = wrapped(line, 12);
        assert_eq!(out[0], " 1 alpha ");
        // Every line after the first starts under the content, not the gutter.
        for cont in &out[1..] {
            assert!(cont.starts_with("   "), "{cont:?} should be indented by 3");
            assert!(!cont.starts_with("    "), "{cont:?} over-indented");
        }
        assert_eq!(out.concat().replace(' ', ""), "1alphabetagammadelta");
    }

    #[test]
    fn a_word_longer_than_the_line_is_broken_hard() {
        let long = "A".repeat(50);
        let line = Line::from(vec![Span::raw("  9 "), Span::raw(long.clone())]);
        let out = wrapped(line, 20);
        assert!(out.len() > 2, "expected several lines, got {out:?}");
        for l in &out {
            assert!(l.width() <= 20, "{l:?} is wider than the pane");
        }
        assert_eq!(out.concat().replace(' ', ""), format!("9{long}"));
    }

    #[test]
    fn json_nesting_is_part_of_the_indent() {
        // gutter " 2 " plus the two-space nesting pad the JSON writer emits.
        let line = Line::from(vec![
            Span::raw(" 2 "),
            Span::raw("  "),
            Span::raw("\"k\": \"vvvvvvvvvvvvvvvvvvvvvvvvv\""),
        ]);
        assert_eq!(hanging_indent(&line), 5);
        let out = wrapped(line, 16);
        assert!(out[1].starts_with("     "), "{:?}", out[1]);
    }

    #[test]
    fn styles_survive_the_break() {
        let keyed = Style::new().fg(Theme::ACCENT);
        let line = Line::from(vec![
            Span::raw(" 1 "),
            Span::styled("x".repeat(40), keyed),
        ]);
        let out = wrap_line(line, 20, 3);
        assert!(out.len() > 1);
        // The indent is unstyled; every cell carrying content keeps its colour.
        for l in &out {
            for span in l.spans.iter().filter(|s| s.content.contains('x')) {
                assert_eq!(span.style, keyed);
            }
        }
    }

    #[test]
    fn a_style_on_the_whole_line_survives_the_break() {
        // The detail pane writes its own lines this way — one bare span under a
        // line-level style — rather than styling each span.
        let line = Line::styled("x".repeat(40), Theme::file());
        let out = wrap_line(line, 20, 0);
        assert!(out.len() > 1);
        for piece in &out {
            assert_eq!(piece.style.fg, Some(Theme::FG));
        }
    }

    #[test]
    fn single_span_lines_have_no_hanging_indent() {
        // A hex dump row or a bare path has no gutter to hang under.
        let line = Line::raw("00000000  a9 38 e9 31  .8.1");
        assert_eq!(hanging_indent(&line), 0);
    }

    #[test]
    fn a_pane_too_narrow_to_wrap_is_left_alone() {
        let line = Line::from(vec![Span::raw(" 1 "), Span::raw("something long")]);
        assert_eq!(wrapped(line, 4).len(), 1);
    }

    // ── the zoomed JSONL view ────────────────────────────────────────────

    #[test]
    fn a_line_that_fits_is_not_truncated() {
        let line = Line::from(vec![Span::raw(" 1 "), Span::raw("hello")]);
        assert_eq!(text(&truncate_line(line, 40)), " 1 hello");
    }

    #[test]
    fn truncation_fills_the_width_exactly_and_marks_the_cut() {
        let line = Line::from(vec![Span::raw(" 1 "), Span::raw("alpha beta gamma")]);
        let cut = truncate_line(line, 10);
        assert_eq!(text(&cut), " 1 alpha …");
        assert_eq!(cut.width(), 10);
    }

    #[test]
    fn truncation_keeps_the_styles_of_what_it_kept() {
        let keyed = Style::new().fg(Theme::ACCENT);
        let line = Line::from(vec![
            Span::raw(" 1 "),
            Span::styled("x".repeat(40), keyed),
        ]);
        let cut = truncate_line(line, 12);
        assert_eq!(cut.width(), 12);
        for span in cut.spans.iter().filter(|s| s.content.contains('x')) {
            assert_eq!(span.style, keyed);
        }
    }

    #[test]
    fn truncation_never_splits_a_wide_character_in_half() {
        let line = Line::from(vec![Span::raw("ab"), Span::raw("我我我")]);
        // Room for one wide character and the ellipsis, not one and a half.
        let cut = truncate_line(line, 5);
        assert_eq!(text(&cut), "ab我…");
        assert_eq!(cut.width(), 5);
    }

    /// A record's own row and its body rows have to start their content in the
    /// same column, or an expanded record reads as two different indents.
    #[test]
    fn the_record_gutter_and_the_body_gutter_are_the_same_width() {
        use crate::app::JsonTok;

        let header = DocRow {
            entry: 6,
            sub: 0,
            cells: vec![(JsonTok::Marker, "▸ ".into())],
            toggle: None,
            parent: None,
            folded: true,
        };
        let body = DocRow {
            entry: 6,
            sub: 1,
            cells: vec![(JsonTok::Punct, "{".into())],
            toggle: None,
            parent: None,
            folded: false,
        };
        let width = |row: &DocRow| {
            let line = jsonl_line(row, 3, false);
            hanging_indent(&line)
        };
        // The header's marker takes the two columns the body's rule does.
        assert_eq!(width(&header) + 2, width(&body));
        assert_eq!(text(&jsonl_line(&header, 3, false)), "  7 ▸ ");
        assert_eq!(text(&jsonl_line(&body, 3, false)), "    │ {");
    }

    /// The JSONL rows a doc lays out to, as `draw_zoom` would build them.
    fn jsonl_rows(doc: &crate::jsonl::Doc) -> Vec<(Line<'static>, bool)> {
        let lit = doc.rows().get(doc.cursor).map(|r| r.entry);
        doc.rows()
            .iter()
            .map(|r| (jsonl_line(r, 2, Some(r.entry) == lit), r.folded))
            .collect()
    }

    fn laid_out(doc: &crate::jsonl::Doc, width: usize) -> Vec<String> {
        zoom_layout(jsonl_rows(doc), doc.cursor, width)
            .iter()
            .map(|(_, line)| text(line))
            .collect()
    }

    #[test]
    fn a_folded_record_stays_one_row_however_long_it_is() {
        let raw = format!(r#"{{"k":"{}"}}"#, "x".repeat(300));
        let doc = crate::jsonl::parse(&format!("{raw}\n"), false);
        let lines = laid_out(&doc, 40);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].ends_with('…'));
        assert_eq!(lines[0].width(), 40);
    }

    #[test]
    fn an_unfolded_value_wraps_instead_of_being_cut() {
        let raw = format!(r#"{{"k":"{}"}}"#, "x".repeat(300));
        let mut doc = crate::jsonl::parse(&format!("{raw}\n"), false);
        doc.toggle_row(0);
        let lines = laid_out(&doc, 40);
        // "▾ {1}", "{", the wrapped "k" member, "}".
        assert!(lines.len() > 4, "{} lines is not wrapped", lines.len());
        assert!(!lines.iter().any(|l| l.ends_with('…')), "{lines:?}");
        for line in &lines {
            assert!(line.width() <= 40, "{line:?} overflows the pane");
        }
        // Every x survives the break.
        let joined: String = lines.concat();
        assert_eq!(joined.matches('x').count(), 300);
    }

    #[test]
    fn a_folded_child_is_cut_even_inside_an_unfolded_record() {
        let raw = format!(r#"{{"k":{{"deep":"{}"}}}}"#, "x".repeat(300));
        let mut doc = crate::jsonl::parse(&format!("{raw}\n"), false);
        doc.toggle_row(0);
        let lines = laid_out(&doc, 40);
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(lines[2].contains("▸ "), "{:?}", lines[2]);
        assert!(lines[2].ends_with('…'));
    }

    #[test]
    fn the_layout_maps_every_screen_line_back_to_its_row() {
        let raw = format!(r#"{{"k":"{}"}}"#, "x".repeat(300));
        let mut doc = crate::jsonl::parse(&format!("{raw}\n{raw}\n"), false);
        doc.toggle_row(0);
        let rows = doc.rows();
        let lines = zoom_layout(jsonl_rows(&doc), doc.cursor, 40);
        // Monotonic, starting at 0, and never naming a row that isn't there.
        assert_eq!(lines[0].0, 0);
        assert!(lines.windows(2).all(|w| w[1].0 == w[0].0 || w[1].0 == w[0].0 + 1));
        assert_eq!(lines.last().unwrap().0, rows.len() - 1);
    }

    #[test]
    fn the_view_follows_the_selection() {
        // Already on screen: nothing moves.
        assert_eq!(reveal(10, 12, 13, 20, 100), 10);
        // Above the top edge.
        assert_eq!(reveal(10, 4, 5, 20, 100), 4);
        // Below the bottom edge: scrolled just far enough.
        assert_eq!(reveal(10, 40, 41, 20, 100), 21);
        // A wrapped row is brought on screen whole where it fits …
        assert_eq!(reveal(0, 15, 20, 20, 100), 0);
        assert_eq!(reveal(0, 25, 30, 20, 100), 10);
        // … and read from its start where it does not.
        assert_eq!(reveal(0, 25, 60, 20, 100), 25);
    }

    #[test]
    fn the_view_never_scrolls_past_the_end() {
        assert_eq!(reveal(90, 95, 96, 20, 100), 80);
        // A body shorter than the pane never scrolls at all.
        assert_eq!(reveal(5, 2, 3, 20, 8), 0);
    }

    /// An app with nothing loaded, for driving the draw path.
    fn test_app() -> App {
        let profile = crate::config::Profile {
            // Nothing is fetched; the client only has to exist.
            endpoint: "http://127.0.0.1:1".into(),
            access_key_id: "key".into(),
            secret_access_key: "secret".into(),
            default_repo: None,
            default_ref: None,
            verify_tls: true,
            timeout_secs: 1,
            description: None,
        };
        let client = crate::lakefs::Client::new(&profile, 500).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            crate::config::Config::default(),
            "test".into(),
            profile,
            client,
            tx,
        )
    }

    /// Draw into a test terminal and read the screen back as trimmed rows.
    fn render(app: &mut App, width: u16, height: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, app, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// The side pane's JSONL records carry the same syntax colours the JSON
    /// preview beside them does, rather than one flat run of text.
    #[tokio::test]
    async fn the_side_pane_colours_jsonl_records() {
        use crate::app::JsonTok;

        let mut app = test_app();
        app.preview.body = Some(PreviewBody::Jsonl(crate::jsonl::parse(
            "{\"a\":1,\"b\":\"x\"}\n",
            false,
        )));

        let lines = preview_lines(&app, 40);
        let record = lines.last().expect("the record's line");
        let styles: Vec<Style> = record.spans.iter().map(|s| s.style).collect();

        assert!(styles.contains(&Theme::json(JsonTok::Key)), "{styles:?}");
        assert!(styles.contains(&Theme::json(JsonTok::Num)), "{styles:?}");
        assert!(styles.contains(&Theme::json(JsonTok::Str)), "{styles:?}");
        // The gutter keeps its own faint style, ahead of the record's cells.
        assert_eq!(record.spans[0].style, Theme::faint());
    }

    /// A `.json` file is one value read top to bottom, so the side pane wraps it
    /// rather than cutting a long string off at the edge. A `.jsonl` record
    /// doesn't — see below.
    #[tokio::test]
    async fn the_side_pane_wraps_a_long_json_value() {
        use crate::app::JsonTok;

        let mut app = test_app();
        let value = "v".repeat(120);
        // The flat rendering the side pane reads, as the parser hands it over:
        // the nesting is a span of its own, ahead of the key.
        app.preview.body = Some(PreviewBody::Json {
            lines: vec![
                vec![(JsonTok::Punct, "{".into())],
                vec![
                    (JsonTok::Punct, "  ".into()),
                    (JsonTok::Key, "\"k\"".into()),
                    (JsonTok::Punct, ": ".into()),
                    (JsonTok::Str, format!("\"{value}\"")),
                ],
                vec![(JsonTok::Punct, "}".into())],
            ],
            doc: crate::jsonl::JsonDoc::new(serde_json::Value::Null),
        });

        let width = 40;
        let lines = preview_lines(&app, width);
        for line in &lines {
            assert!(
                text(line).width() <= width as usize,
                "{:?} overflows the pane",
                text(line)
            );
        }
        // Every character of the value is on show, over as many lines as it took.
        // The indent each continuation hangs under is not part of it.
        let whole: String = lines.iter().map(text).collect::<String>().replace(' ', "");
        assert!(whole.contains(&value), "the value was cut: {whole:?}");
        assert!(
            lines.len() > 4,
            "expected the value to wrap over several lines: {lines:?}"
        );
    }

    /// A record is a row of its own here, and re-flowing one over ten lines
    /// would bury the records under it. The zoom is where they open up.
    #[tokio::test]
    async fn the_side_pane_lets_a_long_jsonl_record_overflow() {
        let mut app = test_app();
        let record = format!(r#"{{"k":"{}"}}"#, "v".repeat(120));
        app.preview.body = Some(PreviewBody::Jsonl(crate::jsonl::parse(&record, false)));

        let lines = preview_lines(&app, 40);
        // One line for the record, however long it is.
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(text(&lines[0]).width() > 40);
    }

    /// A JSON file zoomed: the whole draw path, over the shape the zoom opens
    /// with — the file unfolded all the way down.
    #[tokio::test]
    async fn a_zoomed_json_file_draws_fully_open() {
        let mut app = test_app();
        let value: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":{"c":2}}"#).unwrap();
        app.preview.body = Some(PreviewBody::Json {
            // Only the zoom is under test; the flat lines are the side pane's.
            lines: Vec::new(),
            doc: crate::jsonl::JsonDoc::new(value),
        });
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;

        let all = render(&mut app, 60, 12).join("\n");
        assert!(all.contains("\"a\": 1,"), "{all}");
        // "b" is a container, and it opens with the rest of the file.
        assert!(all.contains(r#"▾ "b": {"#), "{all}");
        assert!(all.contains("\"c\": 2"), "{all}");
        assert!(all.contains(" zoom "), "{all}");

        // `→` on an open row leaves it open: descending is one direction.
        app.move_selection(2);
        app.open();
        let again = render(&mut app, 60, 12).join("\n");
        assert_eq!(again, all, "`→` on an open row folded it");

        // `←` folds it back onto its own row.
        app.back();
        let all = render(&mut app, 60, 12).join("\n");
        assert!(all.contains(r#"▸ "b": {"c": 2}"#), "{all}");
        assert_eq!(app.mode, Mode::Zoom, "folding is not leaving");

        // And `→` opens it again, to its own level.
        app.open();
        let all = render(&mut app, 60, 12).join("\n");
        assert!(all.contains(r#"▾ "b": {"#), "{all}");
        assert!(all.contains("\"c\": 2"), "{all}");
    }

    /// Drives the real draw path — pane frame, layout, clipping and all — over
    /// a JSONL preview, which the pure-function tests above never touch.
    #[tokio::test]
    async fn the_zoomed_view_draws_what_the_cursor_is_on() {
        // Drawn here rather than through `render`, which drops the buffer the
        // selection-background checks below need.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = test_app();

        let mut doc = crate::jsonl::parse("{\"a\":1}\n{\"b\":{\"c\":2}}\n", false);
        doc.cursor = 1;
        doc.toggle_row(1);
        app.preview.body = Some(PreviewBody::Jsonl(doc));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|frame| draw(frame, &mut app, frame.area()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let screen: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        let all = screen.join("\n");

        // The first record stays folded, the second is open to its top level.
        assert!(all.contains(r#"▸ {"a": 1}"#), "{all}");
        assert!(all.contains("▾ {1}"), "{all}");
        assert!(all.contains(r#"▸ "b": {"c": 2}"#), "{all}");
        assert!(all.contains(" zoom "), "{all}");

        // The selected row is the one wearing the selection background, across
        // the whole width of the pane rather than just its text.
        let pane = app.hits.preview.expect("the zoom records its own area");
        let lit: Vec<u16> = (pane.y..pane.y + pane.height)
            .filter(|y| buffer[(pane.x, *y)].bg == Theme::SURFACE)
            .collect();
        assert_eq!(lit.len(), 1, "exactly one row is selected: {lit:?}");
        let row = lit[0];
        assert!(
            screen[row as usize].contains("▾ {1}"),
            "{:?}",
            screen[row as usize]
        );
        assert_eq!(buffer[(pane.x + pane.width - 1, row)].bg, Theme::SURFACE);

        // Every screen line of the pane maps back to a row, for the mouse.
        assert!(!app.hits.preview_rows.is_empty());
        assert!(app.hits.preview_rows.iter().all(|r| *r < 6));
    }

    // ── all at once ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_and_c_level_the_whole_zoomed_document() {
        let mut app = test_app();
        app.preview.body = Some(PreviewBody::Jsonl(crate::jsonl::parse(
            "{\"a\":1,\"m\":{\"p\":2}}\n{\"a\":3,\"m\":{\"p\":4}}\n",
            false,
        )));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;

        // The cursor starts on a folded record, so `a` opens all of it.
        app.expand_all();
        let all = screen(&framed(&mut app, 60, 24));
        assert_eq!(all.matches(r#"▾ "m": {"#).count(), 2, "{all}");
        assert!(all.contains("· unfolded everything"), "{all}");
        assert!(!all.contains("level"), "no level was copied: {all}");

        app.collapse_all();
        let all = screen(&framed(&mut app, 60, 24));
        assert_eq!(all.matches("▸ {").count(), 2, "both records folded: {all}");
        assert!(all.contains("folded everything"), "{all}");

        // Open one record to its top level and stand on it: now `a` has a level
        // to copy, and every record is brought to it rather than opened whole.
        app.open();
        app.expand_all();
        let all = screen(&framed(&mut app, 60, 24));
        assert_eq!(all.matches(r#"▸ "m": {"p":"#).count(), 2, "{all}");
        assert!(all.contains("to level 1"), "{all}");
    }

    /// Nothing outside a foldable zoom has levels, so the keys are quiet there
    /// rather than reporting something that didn't happen.
    #[tokio::test]
    async fn a_and_c_do_nothing_outside_a_foldable_zoom() {
        let mut app = test_app();
        app.status = None;
        app.expand_all();
        app.collapse_all();
        assert!(app.status.is_none());

        // Zoomed, but on a flat body: the same keys have nothing to fold.
        app.preview.body = Some(PreviewBody::Text(vec!["plain".into()]));
        app.mode = Mode::Zoom;
        app.expand_all();
        assert!(app.status.is_none());
    }

    // ── paging ───────────────────────────────────────────────────────────

    /// The pane's top line, as plain text.
    fn top_row(buffer: &ratatui::buffer::Buffer, pane: Rect) -> String {
        (pane.x..pane.x + pane.width)
            .map(|x| buffer[(x, pane.y)].symbol())
            .collect()
    }

    /// `Ctrl-f` / `Ctrl-b` move the view rather than the selection: each press
    /// lands on the page after the one being read, and reads it from the top.
    #[tokio::test]
    async fn a_page_moves_the_view_a_screenful_at_a_time() {
        let mut app = test_app();
        let text: String = (0..40).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        app.preview.body = Some(PreviewBody::Jsonl(crate::jsonl::parse(&text, false)));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;

        let cursor = |app: &App| app.zoom_doc().expect("a foldable zoom").cursor();
        let buffer = framed(&mut app, 60, 24);
        let pane = app.hits.preview.expect("the zoom records its own area");
        // Every record is folded onto a row of one line, so a page is the pane's
        // own height, less the row carried over to the top of the next one.
        let page = pane.height as usize - 1;
        assert!(page > 2, "the pane should hold several records: {page}");
        assert!(top_row(&buffer, pane).contains(r#"{"n": 0}"#));

        app.page(true);
        assert_eq!(cursor(&app), page);
        // The view came with it: the page opens on the row it left off at.
        assert_eq!(app.preview.scroll as usize, page);
        let buffer = framed(&mut app, 60, 24);
        let top = top_row(&buffer, pane);
        assert!(top.contains(&format!(r#"{{"n": {page}}}"#)), "{top:?}");

        app.page(true);
        assert_eq!(cursor(&app), 2 * page);

        // And back the way it came, a page at a time.
        app.page(false);
        assert_eq!(cursor(&app), page);
        app.page(false);
        assert_eq!(cursor(&app), 0);
        assert_eq!(app.preview.scroll, 0);

        // The end of the file stops it rather than the view running off it.
        for _ in 0..20 {
            app.page(true);
        }
        assert_eq!(cursor(&app), 39);
        let buffer = framed(&mut app, 60, 24);
        let all = screen(&buffer);
        assert!(all.contains(r#"{"n": 39}"#), "{all}");
    }

    /// A row taller than the pane can only be read from its start, so paging
    /// steps over it rather than landing back on it.
    #[tokio::test]
    async fn a_page_steps_over_a_row_taller_than_the_pane() {
        let mut app = test_app();
        let wide = format!(r#"{{"k":"{}"}}"#, "x".repeat(2000));
        let mut doc = crate::jsonl::parse(&format!("{wide}\n{{\"n\":1}}\n"), false);
        // Unfolded, the first record wraps well past the height of the pane.
        doc.toggle_row(0);
        app.preview.body = Some(PreviewBody::Jsonl(doc));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;

        framed(&mut app, 60, 24);
        let cursor = |app: &App| app.zoom_doc().expect("a foldable zoom").cursor();
        assert_eq!(cursor(&app), 0);

        app.page(true);
        assert!(cursor(&app) > 0, "the page did not get past the long row");
    }

    /// The wheel scrolls the view straight away and leaves the selection where
    /// it is, carrying it along only when the view would otherwise leave it
    /// behind — at which point it stays on the edge it went out by.
    #[tokio::test]
    async fn the_wheel_scrolls_the_zoom_and_only_then_takes_the_selection() {
        let mut app = test_app();
        let text: String = (0..40).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        app.preview.body = Some(PreviewBody::Jsonl(crate::jsonl::parse(&text, false)));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;

        let cursor = |app: &App| app.zoom_doc().expect("a foldable zoom").cursor();
        let buffer = framed(&mut app, 60, 24);
        let pane = app.hits.preview.expect("the zoom records its own area");
        let height = pane.height as usize;
        // Every record folds onto a row of one line, so lines and rows are the
        // same thing here and the whole file is taller than the pane.
        assert!(height > 6 && height < 40, "unexpected pane height {height}");
        assert!(top_row(&buffer, pane).contains(r#"{"n": 0}"#));

        // A record well inside the view, so the wheel has somewhere to move to
        // before the selection is in its way.
        app.move_selection(5);
        assert_eq!(app.preview.scroll, 0);

        // One notch scrolls, and leaves the selection alone.
        app.mouse_scroll(pane.x, pane.y, true);
        assert_eq!(app.preview.scroll as usize, 3);
        assert_eq!(cursor(&app), 5, "the wheel moved the selection, not the view");

        // The next notch takes the top edge past it, so it comes along and stays
        // there — the top row on screen is the selected one from here on.
        app.mouse_scroll(pane.x, pane.y, true);
        assert_eq!(app.preview.scroll as usize, 6);
        assert_eq!(cursor(&app), 6);
        let buffer = framed(&mut app, 60, 24);
        let top = top_row(&buffer, pane);
        assert!(top.contains(r#"{"n": 6}"#), "{top:?}");

        // Back up: the view moves, and the selection — now well inside it —
        // stays put.
        app.mouse_scroll(pane.x, pane.y, false);
        assert_eq!(app.preview.scroll as usize, 3);
        assert_eq!(cursor(&app), 6);

        // The end of the file stops the view rather than it running off, and the
        // selection settles on the first row of that last screenful.
        for _ in 0..40 {
            app.mouse_scroll(pane.x, pane.y, true);
        }
        assert_eq!(app.preview.scroll as usize, 40 - height);
        assert_eq!(cursor(&app), 40 - height);
        let buffer = framed(&mut app, 60, 24);
        assert!(screen(&buffer).contains(r#"{"n": 39}"#));

        // Scrolling up off a selection at the bottom edge drags it the other way.
        app.select_edge(false);
        framed(&mut app, 60, 24);
        assert_eq!(cursor(&app), 39);
        app.mouse_scroll(pane.x, pane.y, false);
        assert_eq!(app.preview.scroll as usize, 40 - height - 3);
        assert_eq!(cursor(&app), 36, "the selection should hug the bottom edge");
    }

    /// A row taller than the pane can only be shown from its own start, so there
    /// is no scroll position for the wheel to hold. It moves the selection then,
    /// which is the only thing that shifts the view over such a row.
    #[tokio::test]
    async fn the_wheel_gets_past_a_row_taller_than_the_pane() {
        let mut app = test_app();
        let wide = format!(r#"{{"k":"{}"}}"#, "x".repeat(2000));
        let mut doc = crate::jsonl::parse(&format!("{wide}\n{{\"n\":1}}\n"), false);
        doc.toggle_row(0);
        app.preview.body = Some(PreviewBody::Jsonl(doc));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;

        let pane = {
            framed(&mut app, 60, 24);
            app.hits.preview.expect("the zoom records its own area")
        };
        let cursor = |app: &App| app.zoom_doc().expect("a foldable zoom").cursor();
        assert_eq!(cursor(&app), 0);

        app.mouse_scroll(pane.x, pane.y, true);
        assert!(cursor(&app) > 0, "the wheel did not get past the long row");
    }

    /// Paging belongs to the foldable zoom. A flat body is scrolled rather than
    /// selected through, and the panes have `Ctrl-d` and `Ctrl-u` of their own.
    #[tokio::test]
    async fn a_page_does_nothing_outside_a_foldable_zoom() {
        let mut app = test_app();
        app.preview.body = Some(PreviewBody::Text(vec!["plain".into(); 100]));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;

        app.page(true);
        assert_eq!(app.preview.scroll, 0);
    }

    // ── the key-filter menu ──────────────────────────────────────────────

    /// The whole frame: the menu is drawn over the body by `ui::draw`, so the
    /// browser's own `draw` never sees it.
    fn framed(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn screen(buffer: &ratatui::buffer::Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A zoomed JSONL preview whose two records share a shape.
    fn zoomed_jsonl(app: &mut App) {
        let doc = crate::jsonl::parse("{\"a\":1,\"b\":2}\n{\"a\":3,\"b\":4}\n", false);
        app.preview.body = Some(PreviewBody::Jsonl(doc));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;
    }

    #[tokio::test]
    async fn the_key_menu_switches_a_key_off_behind_itself() {
        let mut app = test_app();
        zoomed_jsonl(&mut app);

        app.open_keys();
        assert_eq!(app.mode, Mode::Keys);
        let buffer = framed(&mut app, 60, 20);
        let all = screen(&buffer);
        assert!(all.contains("Filter keys"), "{all}");
        assert!(all.contains("[x] a") && all.contains("[x] b"), "{all}");
        // The zoom is still what is drawn behind the panel.
        assert!(all.contains(r#"▸ {"a": 1, "b": 2}"#), "{all}");

        // The selected key wears the selection bar right across the panel — the
        // menu's side margins are part of its lines for exactly this reason.
        let list = app.keys.list;
        let lit: Vec<u16> = (list.y..list.y + list.height)
            .filter(|y| buffer[(list.x, *y)].bg == Theme::SURFACE)
            .collect();
        assert_eq!(lit, vec![list.y], "one row is selected: {all}");
        assert_eq!(buffer[(list.right() - 1, list.y)].bg, Theme::SURFACE);

        // Switch "b" off: the switch flips, and so do the records around it.
        app.keys_move(1);
        app.keys_toggle();
        let all = screen(&framed(&mut app, 60, 20));
        assert!(all.contains("[ ] b"), "{all}");
        assert!(all.contains(r#"▸ {"a": 1}"#), "{all}");
        // And the pane says what it is holding back, since a key that is simply
        // absent would look exactly the same.
        assert!(all.contains("1 key hidden"), "{all}");

        // Esc puts the filter back as it was.
        app.close_keys(false);
        assert_eq!(
            app.mode,
            Mode::Zoom,
            "the menu closes to the zoom, not to the tree"
        );
        let all = screen(&framed(&mut app, 60, 20));
        assert!(all.contains(r#"▸ {"a": 1, "b": 2}"#), "{all}");
        assert!(!all.contains("key hidden"), "{all}");
    }

    /// The rows of the zoomed document, as plain text. The menu is drawn over
    /// the records, so what they read at is asked of the document rather than
    /// of the screen behind the panel.
    fn doc_rows(app: &App) -> Vec<String> {
        app.jsonl()
            .expect("a zoomed jsonl")
            .rows()
            .iter()
            .map(|row| row.cells.iter().map(|(_, text)| text.as_str()).collect())
            .collect()
    }

    /// A record whose keys sit inside an array. The menu has no level for the
    /// array — it lists the keys inside `"spans": [{…}]` directly under
    /// `spans` — so unfolding a key it lists two deep has to reach three
    /// containers into the record.
    #[tokio::test]
    async fn a_key_inside_an_array_unfolds_the_record_through_it() {
        let mut app = test_app();
        let record = r#"{"spans":[{"name":"x","status":{"code":"OK"}}]}"#;
        app.preview.body = Some(PreviewBody::Jsonl(crate::jsonl::parse(
            &format!("{record}\n"),
            false,
        )));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;

        app.open();
        app.open_keys();
        // The cursor starts on "spans", the only key a folded record shows.
        app.keys_fold(true);
        let rows = doc_rows(&app);
        assert!(
            rows.iter().any(|r| r.contains(r#"▾ "spans": ["#)),
            "{rows:?}"
        );

        // "status" is listed under it now; unfolding that has to reach past the
        // array and its element to the key itself.
        let at = app
            .keys_rows()
            .iter()
            .position(|r| r.key == "status")
            .expect("status is listed under spans");
        app.keys_select(at);
        app.keys_fold(true);

        let rows = doc_rows(&app);
        assert!(
            rows.iter().any(|r| r.contains(r#"▾ "status": {"#)),
            "the record follows the menu through the array: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains(r#""code": "OK""#)),
            "{rows:?}"
        );
    }

    /// The menu and the record keep the same shape: it opens in the shape of the
    /// record the cursor is in, and unfolding a key unfolds that key in it.
    #[tokio::test]
    async fn the_menu_and_the_record_keep_the_same_shape() {
        let mut app = test_app();
        let record = r#"{"a":1,"m":{"p":2,"q":{"r":3}}}"#;
        app.preview.body = Some(PreviewBody::Jsonl(crate::jsonl::parse(
            &format!("{record}\n{record}\n"),
            false,
        )));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;

        // Open the record under the cursor to its top level: the menu opens
        // listing the keys that record is showing, and no deeper.
        app.open();
        app.open_keys();
        let all = screen(&framed(&mut app, 60, 24));
        assert!(all.contains("[x] a") && all.contains("[x] m"), "{all}");
        assert!(!all.contains("[x] p"), "deeper than the record: {all}");

        // Unfolding "m" in the menu unfolds it in the record being read — and
        // there alone: the other record is left folded as it was.
        app.keys_move(1);
        app.keys_fold(true);
        let all = screen(&framed(&mut app, 60, 24));
        assert!(all.contains("[x] p") && all.contains("[x] q"), "{all}");
        let rows = doc_rows(&app);
        assert_eq!(
            rows.iter().filter(|r| r.contains(r#"▾ "m": {"#)).count(),
            1,
            "{rows:?}"
        );
        assert_eq!(
            rows.iter().filter(|r| r.contains(r#"▸ "q": {"#)).count(),
            1,
            "and no further than the menu goes: {rows:?}"
        );
        assert_eq!(
            rows.iter().filter(|r| r.starts_with("▸ {")).count(),
            1,
            "the record that was not being read is untouched: {rows:?}"
        );

        // And folding it back up takes that record with it again.
        app.keys_fold(false);
        let all = screen(&framed(&mut app, 60, 24));
        assert!(!all.contains("[x] p"), "{all}");
        let rows = doc_rows(&app);
        assert_eq!(
            rows.iter().filter(|r| r.contains(r#"▸ "m": {"#)).count(),
            1,
            "{rows:?}"
        );
    }

    /// A record folded onto its own row has no level to lend, so the menu opens
    /// as it always did — the keys the records are made of, one level deep.
    #[tokio::test]
    async fn the_menu_over_a_folded_record_opens_at_its_first_level() {
        let mut app = test_app();
        app.preview.body = Some(PreviewBody::Jsonl(crate::jsonl::parse(
            "{\"a\":1,\"m\":{\"p\":2}}\n",
            false,
        )));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;

        app.open_keys();
        let all = screen(&framed(&mut app, 60, 20));
        assert!(all.contains("[x] a") && all.contains("[x] m"), "{all}");
        assert!(!all.contains("[x] p"), "{all}");
    }

    /// The menu is a panel over the file, not a hole in it: the rows it doesn't
    /// cover keep their records, and only a column either side is cleared.
    #[tokio::test]
    async fn the_menu_does_not_cost_the_rows_around_it() {
        let mut app = test_app();
        let text: String = (0..30)
            .map(|i| format!("{{\"n\":{i},\"m\":{{\"p\":{i}}}}}\n"))
            .collect();
        app.preview.body = Some(PreviewBody::Jsonl(crate::jsonl::parse(&text, false)));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;
        app.open_keys();

        let buffer = framed(&mut app, 80, 24);
        let popup = app.keys.popup;
        let row = |y: u16| -> String {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };

        let above = row(popup.y - 1);
        let below = row(popup.bottom());
        assert!(above.contains(r#""n": "#), "{above:?}");
        assert!(below.contains(r#""n": "#), "{below:?}");

        // The columns either side of it stay clear, so no record runs into the
        // border.
        let y = popup.y + 1;
        assert_eq!(buffer[(popup.x - 1, y)].symbol(), " ");
        assert_eq!(buffer[(popup.right(), y)].symbol(), " ");
        // And clear in the theme's own background, not the terminal's.
        assert_eq!(buffer[(popup.x - 1, y)].bg, Theme::BG);
    }

    #[tokio::test]
    async fn a_click_on_a_menu_line_switches_that_key() {
        let mut app = test_app();
        zoomed_jsonl(&mut app);
        app.open_keys();
        framed(&mut app, 60, 20);

        // The second line of the list, as the draw above placed it.
        let list = app.keys.list;
        assert_eq!(app.keys_row_at(list.x, list.y + 1), Some(1));
        app.keys_select(1);
        app.keys_toggle();
        assert_eq!(app.jsonl().unwrap().keys().hidden(), 1);

        // A click away from the panel is done with it, and keeps the edits.
        assert!(!app.in_keys_popup(0, 0));
        app.close_keys(true);
        assert_eq!(app.jsonl().unwrap().keys().hidden(), 1);
    }

    /// The menu has to fit itself into whatever room the zoom has, and say when
    /// there are more keys than it can show at once.
    #[tokio::test]
    async fn the_menu_fits_itself_into_a_small_terminal() {
        let mut app = test_app();
        let wide: String = (0..12)
            .map(|i| format!(r#""key_number_{i}":{i}"#))
            .collect::<Vec<_>>()
            .join(",");
        app.preview.body = Some(PreviewBody::Jsonl(crate::jsonl::parse(
            &format!("{{{wide}}}\n"),
            false,
        )));
        app.mode = Mode::Zoom;
        app.focus = Focus::Tree;
        app.open_keys();
        app.keys_move(9);

        let all = screen(&framed(&mut app, 46, 14));
        // The selected key is on screen, and the count says what is not.
        assert!(all.contains("[x] key_number_9"), "{all}");
        assert!(all.contains("10/12"), "{all}");
        assert!(
            app.keys.popup.width <= 46 && app.keys.popup.height <= 14,
            "{all}"
        );

        // Back to the top, and the menu scrolls with it.
        app.keys_move(isize::MIN / 2);
        let all = screen(&framed(&mut app, 46, 14));
        assert!(all.contains("[x] key_number_0"), "{all}");
        assert!(all.contains("1/12"), "{all}");
    }

    /// `F` where there are no record keys has nothing to offer, and says so
    /// rather than opening an empty panel.
    #[tokio::test]
    async fn the_menu_only_opens_over_records_with_keys() {
        let mut app = test_app();
        app.open_keys();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.status.as_ref().is_some_and(|s| s.is_error));

        // Zoomed, but on a whole JSON file rather than records.
        let value: serde_json::Value = serde_json::from_str(r#"{"a":1}"#).unwrap();
        app.preview.body = Some(PreviewBody::Json {
            lines: Vec::new(),
            doc: crate::jsonl::JsonDoc::new(value),
        });
        app.mode = Mode::Zoom;
        app.open_keys();
        assert_eq!(app.mode, Mode::Zoom);

        // Records without object keys have nothing to switch off either.
        app.preview.body = Some(PreviewBody::Jsonl(crate::jsonl::parse("1\n2\n", false)));
        app.open_keys();
        assert_eq!(app.mode, Mode::Zoom);
    }
}
