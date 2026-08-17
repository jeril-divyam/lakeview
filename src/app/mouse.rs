//! Mouse input over the rendered frame: hit-testing what the last draw put
//! where, the wheel over the panes and the zoom, clicks and double-clicks,
//! and dragging the pane borders.
//!
//! These are `App`'s own methods, split out because the geometry — handles,
//! grabs, the reveal arithmetic — is a vocabulary of its own. As a child of
//! `app` the module sees the fields it drives without any widening.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crate::ui::{MIN_PREVIEW, MIN_REPOS, MIN_TREE, SCROLL_PADDING};

use super::{App, Focus, Tab, fmt_err};

// ── mouse hit-testing ────────────────────────────────────────────────────

/// A pane border the mouse can take hold of, named for the pane on its left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divider {
    /// Between the repositories pane and the tree, so it moves `ui.repos_width`.
    Repos,
    /// Between the tree and the preview, so it moves the two ratios — and closes
    /// the preview when shoved off the body's edge.
    Tree,
}

/// One of those borders as the last render left it. Everything the drag
/// arithmetic needs is here, so it never works the layout out a second time and
/// gets a different answer than the frame it is moving.
#[derive(Debug, Clone, Copy)]
pub struct Handle {
    pub which: Divider,
    /// The border columns the divider is drawn as: two where two panes meet — the
    /// left pane's right border and the right pane's left — and one where a closed
    /// preview leaves the tree's border against the body's edge.
    pub area: Rect,
    /// Column the pane on the divider's left starts at, so a pointer column minus
    /// this is that pane's width.
    pub start: u16,
    /// Columns from `start` to the end of the body: everything the panes either
    /// side of the divider have to divide between them.
    pub room: u16,
}

/// Screen regions recorded during the last render so mouse events can be
/// mapped back to what was drawn there.
#[derive(Default)]
pub struct Hits {
    /// Inner area of the repositories pane.
    pub repos: Option<Rect>,
    /// Inner area of the tree pane.
    pub tree: Option<Rect>,
    /// Inner area of the detail/preview pane.
    pub preview: Option<Rect>,
    /// When the preview drew a foldable document — a zoom of either kind, or a
    /// `.json` in the side pane — the row each screen line of that area shows.
    /// A row that wrapped occupies several entries. Cleared at the top of every
    /// browse frame, so a frame that draws no rows leaves nothing to click on.
    pub preview_rows: Vec<usize>,
    /// For the same preview, the line each row of the whole document starts at.
    /// The layout is the only account of how tall a row came out, and paging
    /// needs that for rows off screen as well as on it.
    pub preview_row_starts: Vec<usize>,
    /// Lines the whole of that document laid out to, which is what the end of
    /// its scroll is measured against.
    pub preview_lines: usize,
    /// Inner area of the commit list.
    pub commits: Option<Rect>,
    /// (tab, label area) for each tab in the header.
    pub tabs: Vec<(Tab, Rect)>,
    /// The pane borders of the last render, left to right. Empty in any tab or
    /// mode that isn't the three-pane browser, so a border can't be grabbed out
    /// from under something else.
    pub dividers: Vec<Handle>,
    /// The chevron in the repositories pane's bottom border, which folds it down
    /// to its markers and unfolds it again.
    pub repos_toggle: Option<Rect>,
}

impl Hits {
    pub(super) fn hit(area: Rect, col: u16, row: u16) -> bool {
        col >= area.x && col < area.x + area.width && row >= area.y && row < area.y + area.height
    }
}

/// How many lines one wheel notch moves.
pub(super) const WHEEL_LINES: usize = 3;
/// Two clicks on the same cell within this window count as a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// A pane border held by the mouse. The widths follow the pointer as it moves, so
/// this is only what the gesture itself has to remember.
#[derive(Debug, Clone, Copy)]
pub(super) struct Drag {
    which: Divider,
    /// Columns from the border's left cell to where the press landed, so the
    /// border tracks the pointer instead of jumping a column under it.
    grab: u16,
    /// Width the preview had when the press landed, which a repositories|tree drag
    /// holds it to so that only the grabbed border moves. `0` when the preview
    /// wasn't showing.
    preview_w: u16,
    /// Whether the widths ever actually changed, so a click that happens to land
    /// on a border is not a reason to rewrite the config file.
    moved: bool,
}

/// How near the body's right edge the tree|preview border has to be shoved before
/// the preview closes. The preview's own floor already stops the border twenty
/// columns short of the edge, so those columns are the run-up and this is only
/// slack for a terminal that reports the last column as the one before it.
const COLLAPSE_SHOVE: u16 = 2;

impl App {
    // ── mouse ────────────────────────────────────────────────────────────

    /// Which pane a screen cell belongs to, with its inner area.
    fn pane_at(&self, col: u16, row: u16) -> Option<(Focus, Rect)> {
        if let Some(area) = self.hits.repos
            && Hits::hit(area, col, row)
        {
            return Some((Focus::Repos, area));
        }
        if let Some(area) = self.hits.tree
            && Hits::hit(area, col, row)
        {
            return Some((Focus::Tree, area));
        }
        None
    }

    fn row_at(&self, focus: Focus, area: Rect, row: u16) -> Option<usize> {
        let (state, len) = match focus {
            Focus::Repos => (&self.repos.state, self.repos.rows.len()),
            Focus::Tree => (&self.tree.state, self.tree.rows.len()),
            // Not a list: its rows come from the layout, through
            // `hits.preview_rows`, and `pane_at` never names it.
            Focus::Preview => return None,
        };
        let line = state.offset() + (row - area.y) as usize;
        (line < len).then_some(line)
    }

    pub fn mouse_scroll(&mut self, col: u16, row: u16, down: bool) {
        let delta = if down {
            WHEEL_LINES as isize
        } else {
            -(WHEEL_LINES as isize)
        };

        if self.tab == Tab::Commits {
            if self.hits.commits.is_some_and(|a| Hits::hit(a, col, row)) {
                self.move_selection(delta);
            }
            return;
        }

        // Zoomed preview, or the preview pane in the normal layout.
        if self.zoomed() || self.hits.preview.is_some_and(|a| Hits::hit(a, col, row)) {
            if self.focused_doc().is_some() {
                self.wheel_zoom(down);
                return;
            }
            self.preview.scroll = if down {
                self.preview.scroll.saturating_add(WHEEL_LINES as u16)
            } else {
                self.preview.scroll.saturating_sub(WHEEL_LINES as u16)
            };
            return;
        }

        let Some((focus, area)) = self.pane_at(col, row) else {
            return;
        };

        // The focused pane carries its selection along; the other one just peeks,
        // leaving the selection and the focus where they are.
        self.wheel_list(focus, area, down, focus == self.focus);
    }

    /// The wheel over a list pane scrolls the view, and — when `stick` — carries
    /// the selection along only once the view would leave it behind, at which
    /// point it holds to the edge it was about to go out by. This is the wheel a
    /// zoomed document gets, for the same reason: driving the selection instead
    /// would spend the first notches walking it across the pane before anything
    /// scrolled at all.
    ///
    /// Stopping once the last row is on screen means a list shorter than its
    /// viewport doesn't scroll at all.
    ///
    /// Without `stick` the scroll is only as durable as the selection allows:
    /// `List` pulls a selected row that has gone off screen back into view, so a
    /// peek past the selected row is undone by the next frame. Sticking the
    /// selection to the edge is what keeps the render from arguing.
    fn wheel_list(&mut self, focus: Focus, area: Rect, down: bool, stick: bool) {
        let height = area.height as usize;
        let len = match focus {
            Focus::Repos => self.repos.rows.len(),
            Focus::Tree => self.tree.rows.len(),
            // Not a list; the wheel over it goes through `wheel_zoom`.
            Focus::Preview => return,
        };
        if len == 0 || height == 0 {
            return;
        }
        let state = match focus {
            Focus::Repos => &mut self.repos.state,
            _ => &mut self.tree.state,
        };

        let max = len.saturating_sub(height);
        let top = state.offset().min(max);
        let new_top = if down {
            (top + WHEEL_LINES).min(max)
        } else {
            top.saturating_sub(WHEEL_LINES)
        };
        if new_top == top {
            return;
        }
        *state.offset_mut() = new_top;
        if !stick {
            return;
        }

        // A row is one line here, so the view holds exactly `height` of them and
        // the selection has only to be clamped between its edges. Left outside
        // them, the render would drag the view back to it and undo the scroll.
        //
        // Not quite the edges, though: `List` keeps `SCROLL_PADDING` rows of
        // context around the selection, and enforces it by moving the view — so a
        // selection left flush against an edge costs most of the notch. At the
        // ends of the list there is nowhere further to scroll, so it doesn't apply.
        let pad = SCROLL_PADDING.min(height.saturating_sub(1) / 2);
        let last = if new_top == max {
            new_top + height - 1
        } else {
            new_top + height - 1 - pad
        }
        .min(len - 1);
        let first = if new_top == 0 { 0 } else { new_top + pad }.min(last);

        let selected = state.selected().unwrap_or(first);
        let stuck = selected.clamp(first, last);
        if stuck == selected {
            return;
        }
        state.select(Some(stuck));
        match focus {
            Focus::Repos => self.sync_target(),
            // `wheel_list` returns early for the preview, which is no list.
            _ => self.mark_preview_dirty(),
        }
    }

    /// The wheel over a zoomed foldable document scrolls the view, and carries
    /// the selection along only once the view would leave it behind — at which
    /// point it sticks to the edge it was about to go out by. A wheel that drove
    /// the selection instead would spend its first notches crossing the pane
    /// before anything scrolled at all.
    ///
    /// The view is held to whole rows, since a row half on screen is pulled back
    /// into it by the render, so the selection may only be left on a row that
    /// fits entirely between the new view's edges.
    fn wheel_zoom(&mut self, down: bool) {
        let Some(rows_len) = self.focused_doc().map(|doc| doc.rows_len()) else {
            return;
        };
        // Without a frame to measure there is no telling what a notch moves.
        let height = self.hits.preview.map_or(0, |a| a.height as usize);
        if rows_len == 0 || height == 0 || self.hits.preview_row_starts.is_empty() {
            return;
        }

        let max_top = self.hits.preview_lines.saturating_sub(height);
        let top = (self.preview.scroll as usize).min(max_top);
        let new_top = if down {
            (top + WHEEL_LINES).min(max_top)
        } else {
            top.saturating_sub(WHEEL_LINES)
        };
        if new_top == top {
            return;
        }

        let Some((first, last)) = rows_within(
            &self.hits.preview_row_starts,
            self.hits.preview_lines,
            rows_len,
            new_top,
            height,
        ) else {
            // A row taller than the pane fills the view on its own, so no scroll
            // position holds: the render puts the view back to that row's start.
            // Move the selection off it instead, which is the only thing that
            // shifts the view here.
            self.move_selection(if down {
                WHEEL_LINES as isize
            } else {
                -(WHEEL_LINES as isize)
            });
            return;
        };

        let cursor = self.focused_doc().map_or(0, |doc| doc.cursor());
        self.preview.scroll = new_top.min(u16::MAX as usize) as u16;
        if let Some(doc) = self.focused_doc_mut() {
            doc.set_cursor(cursor.clamp(first, last));
        }
    }

    /// Left click: focus the pane and select, or open on a double-click.
    pub fn mouse_click(&mut self, col: u16, row: u16) {
        if let Some(tab) = self
            .hits
            .tabs
            .iter()
            .find(|(_, area)| Hits::hit(*area, col, row))
            .map(|(t, _)| *t)
        {
            self.select_tab(tab);
            return;
        }

        // A control rather than a row, like a tab: it is checked before the
        // double-click bookkeeping so folding the pane can't read as one.
        if self.repos_toggle_at(col, row) {
            self.toggle_repos();
            return;
        }

        let now = Instant::now();
        let double = matches!(
            self.last_click,
            Some((last_col, last_row, at))
                if last_col == col && last_row == row && now.duration_since(at) < DOUBLE_CLICK
        );
        self.last_click = Some((col, row, now));

        if self.tab == Tab::Commits {
            if let Some(area) = self.hits.commits
                && Hits::hit(area, col, row)
            {
                let line = self.commits.state.offset() + (row - area.y) as usize;
                if line < self.commits.commits.len() {
                    self.commits.state.select(Some(line));
                }
            }
            return;
        }

        // A foldable document — the zoom, or the preview pane holding a `.json`.
        // The click focuses the pane and selects the row under the pointer, and
        // the second one folds it: the same gesture every list here answers to.
        // Unzoomed this only speaks for a `.json`; a text preview or a `.jsonl`
        // has no rows and falls through to the panes below.
        if (self.zoomed() || self.preview_folds())
            && let Some(area) = self.hits.preview
            && Hits::hit(area, col, row)
        {
            let Some(&line) = self.hits.preview_rows.get((row - area.y) as usize) else {
                return;
            };
            if !self.zoomed() {
                self.focus = Focus::Preview;
            }
            if let Some(doc) = self.focused_doc_mut() {
                doc.set_cursor(line);
                if double {
                    doc.toggle_row(line);
                }
            }
            return;
        }

        let Some((focus, area)) = self.pane_at(col, row) else {
            return;
        };
        let Some(line) = self.row_at(focus, area, row) else {
            return;
        };

        self.focus = focus;
        match focus {
            Focus::Repos => {
                self.repos.state.select(Some(line));
                self.sync_target();
            }
            // `pane_at` names only the two list panes.
            _ => self.tree.state.select(Some(line)),
        }
        self.mark_preview_dirty();

        if double {
            // A container toggles, so the same gesture closes what it opened;
            // anything else opens — a ref steps right, a file zooms.
            let expandable = match self.focus {
                Focus::Repos => self
                    .repos
                    .selected_row()
                    .is_some_and(|r| r.reference.is_none() && r.expandable),
                _ => self.tree.selected().is_some_and(|n| n.is_dir()),
            };
            if expandable {
                self.toggle();
            } else {
                // `enter` rather than `open`: a double-click is the deliberate
                // gesture, so it is `⏎`'s counterpart and zooms a file.
                self.enter();
            }
        }
    }

    /// Right click mirrors `h`.
    pub fn mouse_back(&mut self) {
        self.back();
    }

    // ── dragging a pane border ───────────────────────────────────────────

    /// The pane border a screen cell belongs to.
    fn handle_at(&self, col: u16, row: u16) -> Option<Handle> {
        self.hits
            .dividers
            .iter()
            .find(|h| Hits::hit(h.area, col, row))
            .copied()
    }

    fn handle(&self, which: Divider) -> Option<Handle> {
        self.hits
            .dividers
            .iter()
            .find(|h| h.which == which)
            .copied()
    }

    /// Columns the preview was laid out with, or `0` when it isn't showing. Read
    /// off the tree's border rather than `hits.preview`, whose padding is no
    /// business of the layout arithmetic. A closed preview leaves that border on
    /// the body's last column, which works out to `0` without a special case.
    fn preview_width(&self) -> u16 {
        let Some(h) = self.handle(Divider::Tree) else {
            return 0;
        };
        let tree_w = (h.area.x + 1).saturating_sub(h.start);
        h.room.saturating_sub(tree_w)
    }

    /// Take hold of a pane border. Answers whether the press landed on one, so a
    /// press that didn't can go on to mean what it usually does: a border is the
    /// one place in the body where a click is not a selection.
    pub fn drag_start(&mut self, col: u16, row: u16) -> bool {
        // Only the browser has borders to move, and the help tab draws over the
        // ones the browser recorded without clearing them.
        if self.tab != Tab::Browse {
            return false;
        }
        let Some(handle) = self.handle_at(col, row) else {
            return false;
        };
        self.drag = Some(Drag {
            which: handle.which,
            grab: col.saturating_sub(handle.area.x),
            preview_w: self.preview_width(),
            moved: false,
        });
        true
    }

    /// Move the held border to the pointer's column.
    ///
    /// The layout is read back out of the last render rather than remembered, so a
    /// border that is no longer there — the tab switched, a zoom opened — ends the
    /// drag rather than moving something that isn't on screen. That, not the
    /// button coming up, is what a stranded drag is caught by.
    pub fn drag_move(&mut self, col: u16) {
        let Some(drag) = self.drag else { return };
        let Some(handle) = self.handle(drag.which) else {
            self.drag = None;
            return;
        };
        let before = self.layout();
        // Where the border's left column is being asked to go, as the width that
        // would give the pane on its left.
        let want = col
            .saturating_sub(drag.grab)
            .saturating_sub(handle.start)
            .saturating_add(1);
        match drag.which {
            Divider::Repos => self.drag_repos(handle, want, col, drag.preview_w),
            Divider::Tree => self.drag_tree(handle, want, col),
        }
        let after = self.layout();
        if let Some(drag) = &mut self.drag {
            drag.moved |= after != before;
        }
    }

    /// The three numbers a drag writes, for telling whether it changed anything.
    fn layout(&self) -> (u16, u16, u16) {
        let ui = &self.cfg.ui;
        (ui.repos_width, ui.tree_ratio, ui.preview_ratio)
    }

    /// The repositories|tree border. The preview keeps the columns it had, so the
    /// border under the pointer is the only one that moves and the tree gives up
    /// or takes back the difference — without that, moving this border would slide
    /// the other one too, the ratios splitting what this border leaves over rather
    /// than the screen. Where the preview is showing, the border also stops short
    /// of crushing it rather than closing it by the back door.
    ///
    /// Shoved against the body's left edge it folds the pane down to its markers,
    /// the mirror of the preview closing at the right, and stays folded until a
    /// whole pane would fit again.
    fn drag_repos(&mut self, handle: Handle, want: u16, col: u16, preview_w: u16) {
        if col < handle.start.saturating_add(COLLAPSE_SHOVE) {
            self.collapse_repos();
            return;
        }
        // Tested against where the pointer is asking to go rather than the width it
        // would be clamped to, which would spring the pane back to its floor the
        // moment the pointer left the edge.
        if self.cfg.ui.repos_width == 0 && want < MIN_REPOS {
            return;
        }
        let keep = MIN_TREE + if self.hits.preview.is_some() { MIN_PREVIEW } else { 0 };
        // `clamp` panics on an inverted range, and every ceiling here inverts on a
        // body too narrow to hold the floors.
        let ceiling = handle.room.saturating_sub(keep).max(MIN_REPOS);
        let repos_w = want.clamp(MIN_REPOS, ceiling);
        self.cfg.ui.repos_width = repos_w;

        // A preview the user has closed is not resurrected by this border.
        if preview_w > 0 {
            let remainder = handle.room.saturating_sub(repos_w);
            let ceiling = remainder.saturating_sub(MIN_TREE).max(MIN_PREVIEW);
            let preview_w = preview_w.clamp(MIN_PREVIEW, ceiling);
            self.cfg.ui.tree_ratio = remainder.saturating_sub(preview_w).max(1);
            self.cfg.ui.preview_ratio = preview_w;
        }
    }

    /// The tree|preview border. The ratios are written as the literal column
    /// counts, which the layout then reproduces exactly: they sum to the room the
    /// two panes divide, so its `room * tree / (tree + preview)` is `tree` on the
    /// nose, and a wider terminal scales them instead.
    ///
    /// Its rightmost legal position leaves the preview at its floor, so
    /// `MIN_PREVIEW` columns of dead travel sit between there and the body's edge:
    /// the border stands still while the pointer crosses them, and only a shove
    /// that arrives at the edge itself closes the preview. Coming back is the same
    /// rule read backwards.
    fn drag_tree(&mut self, handle: Handle, want: u16, col: u16) {
        let last = handle
            .start
            .saturating_add(handle.room)
            .saturating_sub(1);
        if col.saturating_add(COLLAPSE_SHOVE) > last {
            self.cfg.ui.preview_ratio = 0;
            return;
        }
        // Tested against where the pointer is asking to go, not the width it ends
        // up clamped to: the clamp would snap a whole preview open the instant the
        // pointer left the edge, when what is wanted is the run-up in reverse.
        if self.cfg.ui.preview_ratio == 0 && want > handle.room.saturating_sub(MIN_PREVIEW) {
            return;
        }
        let ceiling = handle.room.saturating_sub(MIN_PREVIEW).max(MIN_TREE);
        let tree_w = want.clamp(MIN_TREE, ceiling);
        self.cfg.ui.tree_ratio = tree_w.max(1);
        self.cfg.ui.preview_ratio = handle.room.saturating_sub(tree_w);
    }

    /// Let go. The widths were applied as the pointer moved, so all that is left is
    /// writing them down — and only when something moved, so a click that lands on
    /// a border doesn't touch the file.
    pub fn drag_end(&mut self) {
        if let Some(drag) = self.drag.take()
            && drag.moved
        {
            self.save_layout();
        }
    }

    /// The border being dragged, for the renderer to mark.
    pub fn dragging(&self) -> Option<Divider> {
        self.drag.map(|d| d.which)
    }

    /// Whether the cell is the repositories pane's fold chevron.
    pub fn repos_toggle_at(&self, col: u16, row: u16) -> bool {
        self.hits
            .repos_toggle
            .is_some_and(|a| Hits::hit(a, col, row))
    }

    /// Fold the repositories pane down to its markers, or unfold it again.
    ///
    /// Folded is `repos_width = 0`, the same way a closed preview is
    /// `preview_ratio = 0` — one number the file already understands rather than a
    /// second setting saying the same thing twice.
    pub fn toggle_repos(&mut self) {
        if self.cfg.ui.repos_width == 0 {
            self.cfg.ui.repos_width = self.repos_restore.max(MIN_REPOS);
        } else {
            self.collapse_repos();
        }
        self.save_layout();
    }

    /// Fold the pane, remembering the width to unfold to.
    fn collapse_repos(&mut self) {
        if self.cfg.ui.repos_width > 0 {
            self.repos_restore = self.cfg.ui.repos_width;
        }
        self.cfg.ui.repos_width = 0;
    }

    /// Remember the layout a drag settled on. Failing to is worth a word in the
    /// footer and nothing more: the panes have already moved.
    fn save_layout(&mut self) {
        if let Err(e) = self.cfg.save_layout() {
            self.set_status(fmt_err(e), true);
        }
    }
}

/// The row showing screen line `line`, given the line each row starts at. A line
/// past the end of the body belongs to the last row, which is what the view
/// would be showing there anyway.
pub(super) fn row_at_line(starts: &[usize], line: usize) -> usize {
    starts.partition_point(|start| *start <= line).saturating_sub(1)
}

/// The first and last row lying wholly inside the `height` lines from `top`,
/// given the line each row starts at and the `total` the document laid out to.
///
/// `None` when nothing fits: one row taller than the view covers it all, and
/// there is no row the selection could rest on without the view being dragged
/// back to that row's start.
fn rows_within(
    starts: &[usize],
    total: usize,
    rows_len: usize,
    top: usize,
    height: usize,
) -> Option<(usize, usize)> {
    // The layout is a frame old, so trust it only as far as the document goes.
    let last_row = rows_len.min(starts.len()).checked_sub(1)?;
    let bottom = top + height;
    // A row's lines run up to where the row below it starts; the final row's run
    // to the end of the document.
    let end = |row: usize| starts.get(row + 1).copied().unwrap_or(total);

    let first = starts.partition_point(|start| *start < top);
    if first > last_row || end(first) > bottom {
        return None;
    }
    let mut last = first;
    while last < last_row && end(last + 1) <= bottom {
        last += 1;
    }
    Some((first, last))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── paging the zoom ──────────────────────────────────────────────────

    #[test]
    fn a_line_belongs_to_the_row_it_falls_inside() {
        // Rows 0 and 2 took a line each; row 1 wrapped over three.
        let starts = [0, 1, 4];
        assert_eq!(row_at_line(&starts, 0), 0);
        assert_eq!(row_at_line(&starts, 1), 1);
        assert_eq!(row_at_line(&starts, 3), 1, "still inside the wrapped row");
        assert_eq!(row_at_line(&starts, 4), 2);
    }

    #[test]
    fn a_line_past_the_body_belongs_to_its_last_row() {
        // Paging asks about the line under the bottom edge, which is off the end
        // of a body shorter than the pane.
        assert_eq!(row_at_line(&[0, 1, 4], 99), 2);
    }

    // ── the wheel over the zoom ──────────────────────────────────────────

    #[test]
    fn the_rows_a_view_holds_whole_are_the_ones_inside_its_edges() {
        // Six one-line rows; the view is three lines tall, two lines down.
        let starts = [0, 1, 2, 3, 4, 5];
        assert_eq!(rows_within(&starts, 6, 6, 2, 3), Some((2, 4)));
        // At the top, and at the end where the last row's own lines run out.
        assert_eq!(rows_within(&starts, 6, 6, 0, 3), Some((0, 2)));
        assert_eq!(rows_within(&starts, 6, 6, 3, 3), Some((3, 5)));
    }

    #[test]
    fn a_row_the_view_cuts_in_half_is_not_one_of_them() {
        // Row 1 wraps over lines 1..4. A view of lines 0..3 can't hold it whole,
        // so the selection may only rest on row 0.
        let starts = [0, 1, 4];
        assert_eq!(rows_within(&starts, 5, 3, 0, 3), Some((0, 0)));
        // Started at its second line, the row is cut at the top instead.
        assert_eq!(rows_within(&starts, 5, 3, 2, 3), Some((2, 2)));
    }

    #[test]
    fn a_row_taller_than_the_view_leaves_nowhere_to_rest() {
        // One row over ten lines: whatever the view shows, it shows part of it.
        assert_eq!(rows_within(&[0], 10, 1, 0, 4), None);
        assert_eq!(rows_within(&[0], 10, 1, 3, 4), None);
    }

    #[test]
    fn a_document_shorter_than_the_view_is_held_whole() {
        assert_eq!(rows_within(&[0, 1], 2, 2, 0, 20), Some((0, 1)));
        assert_eq!(rows_within(&[], 0, 0, 0, 20), None);
    }
}
