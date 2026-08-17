//! Folding JSON documents, in the two shapes the preview meets them.
//!
//! [`JsonDoc`] is a whole file, opened all the way down. [`Doc`] is newline-
//! delimited records, each folded onto a row of its own. Both unfold a level at
//! a time over the same [`Open`] tree and the same row vocabulary, and both are
//! driven through [`Folding`], so the zoom's keys don't care which it has.
//!
//! A preview is capped at `preview_bytes`, so unlike the standalone viewer this
//! is ported from, everything is parsed up front and nothing needs a cache.
//!
//! Rows are built as `(JsonTok, String)` runs, so the UI colours them through
//! `Theme::json` and this module stays free of ratatui.

use std::collections::HashMap;

use serde_json::Value;

use crate::keys::KeyFilter;

/// The kinds of token a rendered row is made of, so the UI can colour a row
/// through `Theme::json` without this module knowing about ratatui.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonTok {
    Key,
    Str,
    Num,
    Bool,
    Null,
    Punct,
    /// The `▸` / `▾` a foldable JSONL row carries.
    Marker,
    /// A record that is not valid JSON, and the message saying so.
    Error,
}

/// Columns of a folded value worth building. The pane truncates to its own
/// width; this only has to be wider than any terminal is.
const PREVIEW_BUDGET: usize = 512;
const INDENT: &str = "  ";

/// A run of coloured text making up one row.
pub type Cells = Vec<(JsonTok, String)>;

/// One step of the path from a record's root down to a nested container.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Seg {
    Key(String),
    Index(usize),
}

/// Which containers inside one record are unfolded, as a tree mirroring the
/// value's shape. A child that is missing — or present but not `open` — renders
/// folded, so a record that has just been expanded shows only its top level.
#[derive(Default, Debug, Clone)]
pub struct Open {
    open: bool,
    children: HashMap<Seg, Open>,
}

impl Open {
    /// Flip the node at `path`, creating the nodes leading down to it.
    /// Descendants are kept, so folding and unfolding a container again shows
    /// whatever was open inside it before.
    fn toggle(&mut self, path: &[Seg]) {
        let Some((last, ancestors)) = path.split_last() else {
            return;
        };
        let mut node = self;
        for seg in ancestors {
            node = node.children.entry(seg.clone()).or_default();
        }
        let target = node.children.entry(last.clone()).or_default();
        target.open = !target.open;
    }

    /// Levels unfolded below this node: `0` when nothing under it is open. A
    /// child that is folded stops the count, since nothing under it is on show
    /// however much of it was open before.
    fn depth(&self) -> usize {
        self.children
            .values()
            .filter(|c| c.open)
            .map(|c| 1 + c.depth())
            .max()
            .unwrap_or(0)
    }
}

/// An `Open` tree unfolding every container inside `value` down to `depth`
/// levels, and nothing deeper.
///
/// Level 1 is the value's own members, which are always on show, so nothing
/// opens until level 2. Built from the value rather than edited into the old
/// tree, so what the levels say is what the document shows — a branch somebody
/// had opened deeper folds back to the level like any other.
fn opened_to(value: &Value, depth: usize) -> Open {
    let mut open = Open::default();
    fill(&mut open, value, depth.saturating_sub(1));
    open
}

/// Open every container in `value`, recursing while `levels` remain.
fn fill(open: &mut Open, value: &Value, levels: usize) {
    if levels == 0 {
        return;
    }
    // Only what folds gets a node; a scalar is never asked about.
    let mut descend = |seg: Seg, val: &Value| {
        if brackets(val).is_some() {
            let child = open.children.entry(seg).or_default();
            child.open = true;
            fill(child, val, levels - 1);
        }
    };
    match value {
        Value::Object(map) => {
            for (key, val) in map {
                descend(Seg::Key(key.clone()), val);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                descend(Seg::Index(i), item);
            }
        }
        _ => {}
    }
}

/// The fold state a record takes to match the key menu: a container is open
/// when the menu is listing the keys under the key that names it.
///
/// An array is no level of naming — the menu puts the keys inside
/// `"spans": [{…}]` directly under `spans` — so an array and its elements open
/// with the key itself, however many of them are nested. Counting levels instead
/// would have the two describing different things the moment a record holds a
/// list of objects, which is most of what JSONL is for.
fn opened_like(value: &Value, filter: &KeyFilter) -> Open {
    let mut open = Open::default();
    fill_like(&mut open, value, filter, &mut Vec::new());
    open
}

fn fill_like(open: &mut Open, value: &Value, filter: &KeyFilter, path: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, val) in map {
                // Only what folds is asked about; a scalar has no state to take.
                if brackets(val).is_none() {
                    continue;
                }
                path.push(key.clone());
                if filter.is_open(path) {
                    let child = open.children.entry(Seg::Key(key.clone())).or_default();
                    child.open = true;
                    fill_like(child, val, filter, path);
                }
                path.pop();
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                if brackets(item).is_none() {
                    continue;
                }
                let child = open.children.entry(Seg::Index(i)).or_default();
                child.open = true;
                fill_like(child, item, filter, path);
            }
        }
        _ => {}
    }
}

/// The reverse: the key paths a record has open, named the way the menu names
/// them. Array indices are left out, the menu having no node for one.
fn open_key_paths(value: &Value, open: &Open, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match value {
        Value::Object(map) => {
            for (key, val) in map {
                let Some(child) = child(Some(open), &Seg::Key(key.clone())) else {
                    continue;
                };
                path.push(key.clone());
                out.push(path.clone());
                open_key_paths(val, child, path, out);
                path.pop();
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                if let Some(child) = child(Some(open), &Seg::Index(i)) {
                    open_key_paths(item, child, path, out);
                }
            }
        }
        _ => {}
    }
}

/// The child at `seg` if it is unfolded, `None` if it is folded or untouched.
fn child<'a>(open: Option<&'a Open>, seg: &Seg) -> Option<&'a Open> {
    open?.children.get(seg).filter(|c| c.open)
}

// ── rows ─────────────────────────────────────────────────────────────────

/// One rendered row of a record's body.
pub struct Row {
    pub cells: Cells,
    /// The container this row folds, when folding it does anything.
    pub toggle: Option<Vec<Seg>>,
    /// The container this row sits inside, `None` at the record's top level.
    /// What `←` steps out to once there is nothing left to fold.
    pub parent: Option<Vec<Seg>>,
    /// The row shows a value folded up. Such a row is truncated rather than
    /// wrapped: unfolding it is how you see the rest.
    pub folded: bool,
}

impl Row {
    /// A row at the record's own top level, folding nothing.
    fn plain(cells: Cells) -> Self {
        Self {
            cells,
            toggle: None,
            parent: None,
            folded: false,
        }
    }
}

/// The enclosing container, as a row records it: the top level keeps `None`
/// rather than an empty path, so stepping out of it is plainly a different move.
fn enclosing(path: &[Seg]) -> Option<Vec<Seg>> {
    (!path.is_empty()).then(|| path.to_vec())
}

/// A row of the flattened document, as the cursor and the mouse address it.
pub struct DocRow {
    /// Record this row belongs to.
    pub entry: usize,
    /// `0` on the record's own row, else its 1-based row within the body.
    pub sub: usize,
    pub cells: Cells,
    pub toggle: Option<Vec<Seg>>,
    pub parent: Option<Vec<Seg>>,
    pub folded: bool,
}

fn punct(text: &str) -> (JsonTok, String) {
    (JsonTok::Punct, text.to_string())
}

/// Quote and escape a string the way JSON expects.
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

fn scalar(value: &Value) -> (JsonTok, String) {
    match value {
        Value::Null => (JsonTok::Null, "null".into()),
        Value::Bool(b) => (JsonTok::Bool, b.to_string()),
        Value::Number(n) => (JsonTok::Num, n.to_string()),
        Value::String(s) => (JsonTok::Str, quote(s)),
        _ => unreachable!("scalar called on a container"),
    }
}

/// The bracket pair for a container worth folding, or `None` for anything that
/// renders on a single row.
fn brackets(value: &Value) -> Option<(&'static str, &'static str)> {
    match value {
        Value::Object(map) if !map.is_empty() => Some(("{", "}")),
        Value::Array(items) if !items.is_empty() => Some(("[", "]")),
        _ => None,
    }
}

fn write_body(out: &mut Vec<Row>, value: &Value, open: &Open) {
    match brackets(value) {
        Some((opening, closing)) => {
            out.push(Row::plain(vec![punct(opening)]));
            write_children(out, value, Some(open), 1, &mut Vec::new());
            out.push(Row::plain(vec![punct(closing)]));
        }
        // Scalars and empty containers say all they have to say on one row.
        None => out.push(Row::plain(compact(value, usize::MAX))),
    }
}

fn write_children(
    out: &mut Vec<Row>,
    value: &Value,
    open: Option<&Open>,
    depth: usize,
    path: &mut Vec<Seg>,
) {
    match value {
        Value::Object(map) => {
            for (i, (key, val)) in map.iter().enumerate() {
                let last = i + 1 == map.len();
                write_child(out, Seg::Key(key.clone()), val, open, depth, path, last);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let last = i + 1 == items.len();
                write_child(out, Seg::Index(i), item, open, depth, path, last);
            }
        }
        _ => {}
    }
}

fn write_child(
    out: &mut Vec<Row>,
    seg: Seg,
    value: &Value,
    parent: Option<&Open>,
    depth: usize,
    path: &mut Vec<Seg>,
    last: bool,
) {
    let comma = (!last).then_some(",");

    // Every row here sits inside whatever `path` currently names — the child's
    // own segment is only pushed once we know it opens a block.
    let enclosing = enclosing(path);

    let Some((opening, closing)) = brackets(value) else {
        let mut cells = lead(depth, None);
        push_key(&mut cells, &seg);
        cells.extend(compact(value, usize::MAX));
        cells.extend(comma.map(punct));
        out.push(Row {
            cells,
            toggle: None,
            parent: enclosing,
            folded: false,
        });
        return;
    };

    let open = child(parent, &seg);
    path.push(seg.clone());
    match open {
        Some(open) => {
            let mut cells = lead(depth, Some(true));
            push_key(&mut cells, &seg);
            cells.push(punct(opening));
            out.push(Row {
                cells,
                toggle: Some(path.clone()),
                parent: enclosing.clone(),
                folded: false,
            });

            write_children(out, value, Some(open), depth + 1, path);

            // The closing bracket folds the same container, so a long block can
            // be shut from either end.
            let mut cells = lead(depth, None);
            cells.push(punct(closing));
            cells.extend(comma.map(punct));
            out.push(Row {
                cells,
                toggle: Some(path.clone()),
                parent: enclosing,
                folded: false,
            });
        }
        None => {
            let mut cells = lead(depth, Some(false));
            push_key(&mut cells, &seg);
            cells.extend(compact(value, PREVIEW_BUDGET));
            cells.extend(comma.map(punct));
            out.push(Row {
                cells,
                toggle: Some(path.clone()),
                parent: enclosing,
                folded: true,
            });
        }
    }
    path.pop();
}

/// Indentation for a row at `depth`, with the fold marker — when the row has
/// one — taking the two columns just before the content, so a foldable row stays
/// aligned with its plain siblings.
fn lead(depth: usize, marker: Option<bool>) -> Cells {
    let mut cells = Vec::with_capacity(2);
    let pad = match marker {
        Some(_) => INDENT.repeat(depth - 1),
        None => INDENT.repeat(depth),
    };
    if !pad.is_empty() {
        cells.push(punct(&pad));
    }
    if let Some(open) = marker {
        cells.push((
            JsonTok::Marker,
            if open { "▾ ".into() } else { "▸ ".into() },
        ));
    }
    cells
}

/// Prefix an object member with its key; array elements have none.
fn push_key(cells: &mut Cells, seg: &Seg) {
    if let Seg::Key(key) = seg {
        cells.push((JsonTok::Key, quote(key)));
        cells.push(punct(": "));
    }
}

/// Render `value` on a single line, giving up once `budget` columns are used.
fn compact(value: &Value, budget: usize) -> Cells {
    let mut out = Vec::new();
    let mut used = 0usize;
    write_compact(&mut out, &mut used, budget, value);
    out
}

/// Append `text`, cut short if it would run past `budget`. Clipping here rather
/// than only between items keeps one enormous string from costing as much as
/// the whole record — a folded row is truncated to the pane anyway.
fn emit(out: &mut Cells, used: &mut usize, budget: usize, tok: JsonTok, text: String) {
    let room = budget.saturating_sub(*used);
    if text.chars().count() <= room {
        *used += text.chars().count();
        out.push((tok, text));
        return;
    }
    let mut cut: String = text.chars().take(room).collect();
    cut.push('…');
    *used = budget;
    out.push((tok, cut));
}

fn write_compact(out: &mut Cells, used: &mut usize, budget: usize, value: &Value) {
    if *used >= budget {
        return;
    }
    // The closing bracket is emitted past the budget on purpose, so a folded
    // preview still reads as the container it stands for.
    let close = |out: &mut Cells, text: &str| out.push(punct(text));
    match value {
        Value::Array(items) if items.is_empty() => {
            emit(out, used, budget, JsonTok::Punct, "[]".into())
        }
        Value::Array(items) => {
            emit(out, used, budget, JsonTok::Punct, "[".into());
            for (i, item) in items.iter().enumerate() {
                if *used >= budget {
                    close(out, "…");
                    break;
                }
                if i > 0 {
                    emit(out, used, budget, JsonTok::Punct, ", ".into());
                }
                write_compact(out, used, budget, item);
            }
            close(out, "]");
        }
        Value::Object(map) if map.is_empty() => {
            emit(out, used, budget, JsonTok::Punct, "{}".into())
        }
        Value::Object(map) => {
            emit(out, used, budget, JsonTok::Punct, "{".into());
            for (i, (key, val)) in map.iter().enumerate() {
                if *used >= budget {
                    close(out, "…");
                    break;
                }
                if i > 0 {
                    emit(out, used, budget, JsonTok::Punct, ", ".into());
                }
                emit(out, used, budget, JsonTok::Key, quote(key));
                emit(out, used, budget, JsonTok::Punct, ": ".into());
                write_compact(out, used, budget, val);
            }
            close(out, "}");
        }
        value => {
            let (tok, text) = scalar(value);
            emit(out, used, budget, tok, text);
        }
    }
}

/// A short label describing the shape of a value, e.g. `{5}` or `[12]`.
fn shape(value: &Value) -> String {
    match value {
        Value::Object(map) => format!("{{{}}}", map.len()),
        Value::Array(items) => format!("[{}]", items.len()),
        Value::String(_) => "str".into(),
        Value::Number(_) => "num".into(),
        Value::Bool(_) => "bool".into(),
        Value::Null => "null".into(),
    }
}

// ── driving a document from the zoom ─────────────────────────────────────

/// A zoomed document the cursor can be driven through: rows that fold, a
/// selection, and a `←` that winds out of whatever is open.
///
/// Both document kinds implement it, so the zoom moves, folds and backs out of
/// either without knowing which one it is showing.
pub trait Folding {
    /// Rows currently on show — folding changes this, so it is recomputed.
    fn rows_len(&self) -> usize;
    fn cursor(&self) -> usize;
    fn set_cursor(&mut self, row: usize);
    /// Unfold or fold whatever `row` heads, and select it.
    fn toggle_row(&mut self, row: usize);
    /// `←` — fold what is open, else step out. `false` once the cursor is at the
    /// document's own level with nothing left to close. Winding a document shut
    /// is as far as `←` goes: leaving the zoom is `Esc`'s.
    fn back(&mut self) -> bool;

    /// Unfold everything to `depth` levels and fold whatever is deeper, so the
    /// whole document reads at one level however it was folded before. `0` folds
    /// it up altogether; a document whose own brackets don't fold treats that
    /// as `1`, its shallowest.
    fn expand_to(&mut self, depth: usize);

    /// What `a` should unfold to: `Some(level)` to bring the whole document up to
    /// the level the cursor is reading at, `None` for all of it, however deep.
    fn expand_target(&self) -> Option<usize>;

    /// `a` — unfold, either as far as it goes or to the level the cursor names.
    /// Returns the level everything now reads at, or `None` for all of it.
    fn expand_all(&mut self) -> Option<usize> {
        let target = self.expand_target();
        self.expand_to(target.unwrap_or(usize::MAX));
        target
    }

    /// `c` — fold everything back up, however deep any of it was open.
    fn collapse_all(&mut self) {
        self.expand_to(0);
    }

    fn move_cursor(&mut self, delta: isize) {
        let last = self.rows_len().saturating_sub(1) as isize;
        let next = (self.cursor() as isize + delta).clamp(0, last) as usize;
        self.set_cursor(next);
    }

    /// Pull the cursor back inside the document after a fold, a filter edit or
    /// a restore changed how many rows are on show. Asking is not free —
    /// `rows_len` rebuilds the rows — so shape changes call this once, at the
    /// end.
    fn clamp_cursor(&mut self) {
        let last = self.rows_len().saturating_sub(1);
        if self.cursor() > last {
            self.set_cursor(last);
        }
    }

    fn select_edge(&mut self, first: bool) {
        let row = if first {
            0
        } else {
            self.rows_len().saturating_sub(1)
        };
        self.set_cursor(row);
    }

    fn toggle_cursor(&mut self) {
        self.toggle_row(self.cursor());
    }

    /// Whether `row` shows something folded up — something `→` has to open.
    fn folded(&self, row: usize) -> bool;

    /// `→` — unfold what the cursor is on, and only unfold. Descending is one
    /// direction: a key that shuts what it just opened cannot be held down, and
    /// it leaves you having to look at a row to know what pressing it will do.
    /// `←` folds, and `space` is the one that does both.
    fn expand_cursor(&mut self) {
        if self.folded(self.cursor()) {
            self.toggle_row(self.cursor());
        }
    }
}

// ── a whole JSON file ────────────────────────────────────────────────────

/// One JSON document, folded a level at a time.
///
/// It opens unfolded all the way down. A file is one value, and reading it is
/// reading the whole of it — unlike JSONL, where the records repeat a shape and
/// the top level is the thing worth seeing first. `c` folds it back up.
///
/// The root brackets themselves don't fold. Collapsing a whole file to `{…}`
/// says nothing, and `←` at that level is better spent leaving the zoom.
pub struct JsonDoc {
    value: Value,
    open: Open,
    /// Selected row, indexing `rows()`.
    pub cursor: usize,
}

impl JsonDoc {
    pub fn new(value: Value) -> Self {
        let open = opened_to(&value, usize::MAX);
        Self {
            value,
            open,
            cursor: 0,
        }
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        write_body(&mut out, &self.value, &self.open);
        out
    }

    /// The fold shape, to be put back on a later visit to the same object.
    pub fn folds(&self) -> Open {
        self.open.clone()
    }

    /// Adopt a fold shape kept from an earlier visit. The tree is only ever
    /// consulted by path, so segments naming keys this value no longer has are
    /// simply never looked up — a changed object opens the way it would have
    /// anyway rather than misbehaving.
    pub fn restore(&mut self, open: Open) {
        self.open = open;
        self.clamp_cursor();
    }
}

impl Folding for JsonDoc {
    fn rows_len(&self) -> usize {
        self.rows().len()
    }

    fn cursor(&self) -> usize {
        self.cursor
    }

    fn set_cursor(&mut self, row: usize) {
        self.cursor = row;
    }

    fn folded(&self, row: usize) -> bool {
        self.rows().get(row).is_some_and(|r| r.folded)
    }

    fn toggle_row(&mut self, row: usize) {
        let rows = self.rows();
        let Some(target) = rows.get(row) else {
            return;
        };
        self.cursor = row;
        let Some(path) = target.toggle.clone() else {
            return;
        };
        // A container is folded by its closing bracket as readily as by its
        // opening row; land on the row that survives either way. Paths are
        // absolute, so the first row carrying this one is that opening row.
        if let Some(opening) = rows.iter().position(|r| r.toggle.as_ref() == Some(&path)) {
            self.cursor = opening;
        }
        self.open.toggle(&path);
        self.clamp_cursor();
    }

    /// The root brackets don't fold, so the file's shallowest level is 1 and a
    /// `depth` of 0 means the same thing.
    fn expand_to(&mut self, depth: usize) {
        self.open = opened_to(&self.value, depth);
        self.clamp_cursor();
    }

    /// A file is one value, not a shape repeated, so there is nothing to level
    /// it against: `a` opens the whole of it and `c` shuts it back to level 1.
    fn expand_target(&self) -> Option<usize> {
        None
    }

    fn back(&mut self) -> bool {
        let rows = self.rows();
        let Some(row) = rows.get(self.cursor) else {
            return false;
        };

        // A block that is open closes, from either end.
        if row.toggle.is_some() && !row.folded {
            self.toggle_row(self.cursor);
            return true;
        }

        // Nothing to close here, so step out to the block this row sits in. At
        // the document's own level there is no such block, and the root brackets
        // being unfoldable, that is as far as `←` reaches.
        let Some(path) = row.parent.clone() else {
            return false;
        };
        self.cursor = rows
            .iter()
            .position(|r| r.toggle.as_ref() == Some(&path))
            .unwrap_or(self.cursor);
        true
    }
}

// ── the JSONL document ───────────────────────────────────────────────────

pub struct Entry {
    /// The record's text, verbatim.
    pub raw: String,
    value: Option<Value>,
    /// The value with the filtered-out keys dropped, when the filter hides any.
    /// Pruning once per edit rather than once per frame keeps every row cheap,
    /// and leaves `value` intact for a filter that is switched back on.
    view: Option<Value>,
    /// The parse failure, when this record is not valid JSON.
    error: Option<String>,
    expanded: bool,
    open: Open,
}

impl Entry {
    fn new(raw: String) -> Self {
        let (value, error) = match serde_json::from_str::<Value>(&raw) {
            Ok(value) => (Some(value), None),
            Err(e) => (None, Some(e.to_string())),
        };
        Self {
            raw,
            value,
            view: None,
            error,
            expanded: false,
            open: Open::default(),
        }
    }

    /// The value as it should be shown: pruned when the filter hides anything.
    /// Everything that renders a record goes through here, so no caller has to
    /// know that filtering exists.
    fn shown(&self) -> Option<&Value> {
        self.view.as_ref().or(self.value.as_ref())
    }

    /// The record's own row: its folded preview, or a shape hint once the body
    /// below is already showing the value.
    fn header(&self) -> Cells {
        let marker = if self.expanded { "▾ " } else { "▸ " };
        let marker_tok = if self.error.is_some() {
            JsonTok::Error
        } else {
            JsonTok::Marker
        };
        let mut cells = vec![(marker_tok, marker.to_string())];

        match (self.shown(), &self.error) {
            (_, Some(_)) if self.expanded => cells.push((JsonTok::Null, "err".into())),
            (_, Some(err)) => {
                cells.push((JsonTok::Error, self.raw.clone()));
                cells.push((JsonTok::Null, format!("   ({err})")));
            }
            (Some(value), _) if self.expanded => {
                cells.push((JsonTok::Null, shape(value)))
            }
            (Some(value), _) => cells.extend(compact(value, PREVIEW_BUDGET)),
            (None, None) => {}
        }
        cells
    }

    /// The record as the side pane shows it: one coloured line, whatever the
    /// zoom has folded or unfolded — the two views share a document, not a
    /// cursor, and the pane has no fold state of its own to show.
    ///
    /// The text is re-spaced by `compact` rather than kept verbatim, which is
    /// what lets it be coloured at all, and matches the whole-file JSON preview
    /// beside it. A record that doesn't parse keeps its raw text and its error.
    ///
    /// Keys switched off in the zoom's filter are gone from here too — the pane
    /// and the zoom share one document, and a line that still showed them would
    /// contradict the view it belongs to.
    pub fn line(&self) -> Cells {
        match (self.shown(), &self.error) {
            (_, Some(err)) => vec![
                (JsonTok::Error, self.raw.clone()),
                (JsonTok::Null, format!("   ({err})")),
            ],
            (Some(value), _) => compact(value, PREVIEW_BUDGET),
            (None, None) => Vec::new(),
        }
    }

    fn body(&self) -> Vec<Row> {
        if let Some(err) = &self.error {
            return vec![
                Row::plain(vec![(JsonTok::Error, format!("invalid JSON: {err}"))]),
                Row::plain(vec![(JsonTok::Error, self.raw.clone())]),
            ];
        }
        let mut out = Vec::new();
        if let Some(value) = self.shown() {
            write_body(&mut out, value, &self.open);
        }
        out
    }
}

/// A file of newline-delimited records, each folding on its own, and the key
/// filter they are all shown through.
pub struct Doc {
    pub entries: Vec<Entry>,
    /// Selected row, indexing `rows()`.
    pub cursor: usize,
    /// The fetch stopped at `preview_bytes`, so this is not the whole file.
    pub truncated: bool,
    /// Which of the records' keys are shown, edited through the zoom's menu.
    filter: KeyFilter,
}

/// Split `text` into records. A capped fetch usually stops mid-record, so the
/// fragment is dropped rather than reported as a parse error the file does not
/// actually have.
pub fn parse(text: &str, truncated: bool) -> Doc {
    let mut lines: Vec<&str> = text.lines().collect();
    if truncated && !text.ends_with('\n') {
        lines.pop();
    }
    let entries: Vec<Entry> = lines
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Entry::new(line.to_string()))
        .collect();
    // The records are parsed already, so the key structure costs a walk over
    // values rather than a second pass over the text.
    let filter = KeyFilter::discover(entries.iter().filter_map(|e| e.value.as_ref()));
    Doc {
        entries,
        cursor: 0,
        truncated,
        filter,
    }
}

impl Doc {
    /// The whole document flattened: every record's own row, plus the body rows
    /// of those that are expanded.
    pub fn rows(&self) -> Vec<DocRow> {
        let mut out = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            out.push(DocRow {
                entry: i,
                sub: 0,
                cells: entry.header(),
                toggle: None,
                parent: None,
                folded: !entry.expanded,
            });
            if !entry.expanded {
                continue;
            }
            for (j, row) in entry.body().into_iter().enumerate() {
                out.push(DocRow {
                    entry: i,
                    sub: j + 1,
                    cells: row.cells,
                    toggle: row.toggle,
                    parent: row.parent,
                    folded: row.folded,
                });
            }
        }
        out
    }

    pub fn keys(&self) -> &KeyFilter {
        &self.filter
    }

    /// Edit which keys are shown, and re-prune every record with the result.
    ///
    /// Pruning here, once, is what lets `header`, `body` and `line` stay
    /// ignorant of filtering. The cursor is pulled back in afterwards: hiding a
    /// key can shorten an unfolded record out from under it.
    pub fn edit_keys(&mut self, edit: impl FnOnce(&mut KeyFilter)) {
        edit(&mut self.filter);
        let filter = &self.filter;
        let hiding = filter.hidden() > 0;
        for entry in &mut self.entries {
            entry.view = match (&entry.value, hiding) {
                (Some(value), true) => {
                    let mut pruned = value.clone();
                    filter.prune(&mut pruned);
                    Some(pruned)
                }
                // Nothing hidden: the parsed value is what to show, and holding
                // a copy of it would only be a copy.
                _ => None,
            };
        }
        self.clamp_cursor();
    }

    /// How deep record `entry` is unfolded: `0` when it is folded onto its own
    /// row, `1` when it is open with everything inside it folded, and so on.
    fn record_depth(&self, entry: usize) -> usize {
        let entry = &self.entries[entry];
        if entry.expanded {
            1 + entry.open.depth()
        } else {
            0
        }
    }

    /// Put the cursor back on `entry`'s own row after something has changed the
    /// shape of the whole document under it. Expanding or folding every record
    /// at once moves every row, and the record you were reading is the one thing
    /// worth holding on to.
    fn land_on(&mut self, entry: Option<usize>) {
        let row = entry.and_then(|entry| {
            self.rows()
                .iter()
                .position(|r| r.entry == entry && r.sub == 0)
        });
        match row {
            // A position in the rows just built is in range by construction,
            // so it needs no clamp — and the clamp's rebuild is not free.
            Some(row) => self.cursor = row,
            None => self.clamp_cursor(),
        }
    }

    /// Unfold or fold a key in the menu, reporting whether anything moved. Kept
    /// apart from `edit_keys` because the menu's own shape changes nothing about
    /// what the records show, and re-pruning the file to fold a row would be
    /// work for nothing.
    pub fn fold_keys(&mut self, path: &[usize], open: bool) -> bool {
        self.filter.set_open(path, open)
    }

    /// The record the cursor is in.
    pub fn cursor_entry(&self) -> Option<usize> {
        self.rows().get(self.cursor).map(|row| row.entry)
    }

    /// Open the menu to match a record: a key whose value that record shows
    /// open is a key the menu lists the keys under. What `F` does on the way in,
    /// so the menu opens describing what is already on screen.
    ///
    /// Nothing about what the records show changes, so — like `fold_keys` — the
    /// file is not re-pruned.
    pub fn open_keys_to(&mut self, entry: usize) {
        let mut paths = Vec::new();
        if let Some(record) = self.entries.get(entry)
            && let Some(value) = record.shown()
        {
            open_key_paths(value, &record.open, &mut Vec::new(), &mut paths);
        }
        self.filter.open_only(&paths);
    }

    /// The same sync the other way: open one record to match the menu, leaving
    /// the rest of the file as it was. What `←`/`→` in the menu moves — the
    /// whole file is `a`'s business, not the menu's.
    pub fn open_entry_to_keys(&mut self, entry: usize) {
        let Some(record) = self.entries.get(entry) else {
            return;
        };
        let open = match record.shown() {
            Some(value) => opened_like(value, &self.filter),
            None => Open::default(),
        };
        let record = &mut self.entries[entry];
        // The record itself has to be open for any of this to be on show. A menu
        // with nothing unfolded leaves it at its top level, which is the shape
        // the menu's own roots describe.
        record.expanded = true;
        record.open = open;
        // Its rows have all been rebuilt under the cursor, and everything below
        // it has shifted; the record is what there is to hold on to.
        self.land_on(Some(entry));
    }
}

impl Folding for Doc {
    fn rows_len(&self) -> usize {
        self.rows().len()
    }

    fn cursor(&self) -> usize {
        self.cursor
    }

    fn set_cursor(&mut self, row: usize) {
        self.cursor = row;
    }

    /// A record folded onto its own row counts as folded, like any other row
    /// showing a value it has not opened.
    fn folded(&self, row: usize) -> bool {
        self.rows().get(row).is_some_and(|r| r.folded)
    }

    /// A record's own row folds the whole record; a body row folds the container
    /// it names, one level at a time.
    fn toggle_row(&mut self, row: usize) {
        let rows = self.rows();
        let Some(target) = rows.get(row) else {
            return;
        };
        let (entry, sub) = (target.entry, target.sub);
        self.cursor = row;

        if sub == 0 {
            self.entries[entry].expanded = !self.entries[entry].expanded;
        } else if let Some(path) = target.toggle.clone() {
            // A container is folded by its closing bracket as readily as by its
            // opening row; land on the row that survives either way.
            if let Some(opening) = rows
                .iter()
                .position(|r| r.entry == entry && r.toggle.as_ref() == Some(&path))
            {
                self.cursor = opening;
            }
            self.entries[entry].open.toggle(&path);
        }
        self.clamp_cursor();
    }

    /// Every record to the same level: `depth` of 0 folds them all onto their
    /// own rows, 1 opens each to its top level with everything inside folded,
    /// and so on down.
    ///
    /// A record that doesn't parse has no levels — it expands to its error and
    /// its raw text, or folds back onto its row, and nothing in between.
    fn expand_to(&mut self, depth: usize) {
        let anchor = self.rows().get(self.cursor).map(|r| r.entry);
        for entry in &mut self.entries {
            entry.expanded = depth > 0;
            entry.open = match &entry.value {
                Some(value) => opened_to(value, depth),
                None => Open::default(),
            };
        }
        self.land_on(anchor);
    }

    /// The level to copy is the one the cursor's *record* reads at, from
    /// wherever inside it the cursor sits — the folded rows within an open
    /// record included. A record open to level 2 with the cursor on a folded row
    /// inside it is still a record open to level 2, and "the rest like this one"
    /// is what `a` means anywhere in it.
    ///
    /// Only a record folded onto its own row has no level to copy. There `a`
    /// means "open all of this", and the file being a shape repeated, all of it
    /// means all of every record.
    fn expand_target(&self) -> Option<usize> {
        let rows = self.rows();
        let row = rows.get(self.cursor)?;
        match self.record_depth(row.entry) {
            0 => None,
            depth => Some(depth),
        }
    }

    /// Ascends the way it does in the tree, and gives way only once the record
    /// the cursor sits in is folded back up.
    fn back(&mut self) -> bool {
        let rows = self.rows();
        let Some(row) = rows.get(self.cursor) else {
            return false;
        };
        let entry = row.entry;

        // The record itself: fold it up, or — already folded — stop here.
        if row.sub == 0 {
            if !self.entries[entry].expanded {
                return false;
            }
            self.entries[entry].expanded = false;
            self.clamp_cursor();
            return true;
        }

        // A block that is open closes, from its opening row or its closing
        // bracket alike.
        if row.toggle.is_some() && !row.folded {
            self.toggle_row(self.cursor);
            return true;
        }

        // Nothing to close here, so step out to the block this row sits in —
        // or to the record's own row, at its top level.
        let header = rows.iter().position(|r| r.entry == entry && r.sub == 0);
        self.cursor = row
            .parent
            .as_ref()
            .and_then(|path| {
                rows.iter()
                    .position(|r| r.entry == entry && r.toggle.as_ref() == Some(path))
            })
            .or(header)
            .unwrap_or(self.cursor);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(lines: &[&str]) -> Doc {
        parse(&format!("{}\n", lines.join("\n")), false)
    }

    fn rendered(doc: &Doc) -> Vec<String> {
        doc.rows()
            .iter()
            .map(|r| r.cells.iter().map(|(_, s)| s.as_str()).collect())
            .collect()
    }

    /// One record as the side pane draws it.
    fn line(entry: &Entry) -> String {
        entry.line().iter().map(|(_, s)| s.as_str()).collect()
    }

    #[test]
    fn every_record_starts_folded_on_one_row() {
        let doc = doc(&[r#"{"a":1,"b":{"c":2}}"#, r#"{"a":2}"#]);
        assert_eq!(
            rendered(&doc),
            [r#"▸ {"a": 1, "b": {"c": 2}}"#, r#"▸ {"a": 2}"#]
        );
    }

    #[test]
    fn blank_lines_are_not_records() {
        let doc = parse("{\"a\":1}\n\n   \n{\"a\":2}\n", false);
        assert_eq!(doc.entries.len(), 2);
    }

    #[test]
    fn a_truncated_tail_is_dropped_but_a_whole_one_is_kept() {
        assert_eq!(parse("{\"a\":1}\n{\"a\":2", true).entries.len(), 1);
        assert_eq!(parse("{\"a\":1}\n{\"a\":2}\n", true).entries.len(), 2);
        // Without the cap the fragment is real, and shows as a parse error.
        assert_eq!(parse("{\"a\":1}\n{\"a\":2", false).entries.len(), 2);
    }

    #[test]
    fn expanding_a_record_shows_only_its_top_level() {
        let mut doc = doc(&[r#"{"a":1,"b":{"c":2},"d":[1,2]}"#]);
        doc.toggle_row(0);
        assert_eq!(
            rendered(&doc),
            [
                "▾ {3}",
                "{",
                "  \"a\": 1,",
                "▸ \"b\": {\"c\": 2},",
                "▸ \"d\": [1, 2]",
                "}",
            ]
        );
    }

    #[test]
    fn folded_rows_truncate_and_open_ones_wrap() {
        let mut doc = doc(&[r#"{"b":{"c":2}}"#]);
        // Folded: the record's own row, and the nested container inside it.
        assert!(doc.rows()[0].folded);
        doc.toggle_row(0);
        let rows = doc.rows();
        assert!(!rows[0].folded, "an expanded record shows a shape hint");
        assert!(rows[2].folded, "the folded child is truncated, not wrapped");
        assert!(!rows[1].folded, "the opening brace has nothing to hide");
    }

    #[test]
    fn unfolding_a_child_reveals_its_own_level_only() {
        let mut doc = doc(&[r#"{"a":{"b":{"c":1}}}"#]);
        doc.toggle_row(0);
        doc.toggle_row(2); // "a"
        assert_eq!(
            rendered(&doc),
            ["▾ {1}", "{", "▾ \"a\": {", "  ▸ \"b\": {\"c\": 1}", "  }", "}"]
        );
    }

    /// `→` only ever opens. Pressing it again on what it just opened has to
    /// leave it open, or the key means two different things a row apart.
    #[test]
    fn expanding_twice_leaves_it_open() {
        let mut doc = nested();
        doc.expand_cursor();
        assert_eq!(doc.record_depth(0), 1);
        doc.expand_cursor();
        assert_eq!(doc.record_depth(0), 1, "still open");

        // The same a level in: open "m", press again, still open.
        let m = doc
            .rows()
            .iter()
            .position(|r| r.folded && r.cells.iter().any(|(_, t)| t.contains("\"m\"")))
            .expect("the folded \"m\" row");
        doc.set_cursor(m);
        doc.expand_cursor();
        assert_eq!(doc.record_depth(0), 2);
        doc.expand_cursor();
        assert_eq!(doc.record_depth(0), 2, "still open");

        // `space` is the key that does both, and still folds it back up.
        doc.toggle_cursor();
        assert_eq!(doc.record_depth(0), 1);
    }

    #[test]
    fn json_expanding_twice_leaves_it_open() {
        let mut doc = json(r#"{"a":{"b":1}}"#);
        doc.set_cursor(1);
        doc.expand_cursor();
        let open = json_rendered(&doc);
        assert!(open.iter().any(|r| r.contains(r#"▾ "a": {"#)), "{open:?}");

        doc.expand_cursor();
        assert_eq!(json_rendered(&doc), open, "pressing it again changes nothing");

        // A closing bracket folds its block for `←` and `space`; `→` there is
        // not a way to fold it either.
        doc.set_cursor(3);
        doc.expand_cursor();
        assert_eq!(json_rendered(&doc), open);
    }

    #[test]
    fn the_closing_bracket_folds_the_same_container() {
        let mut doc = doc(&[r#"{"a":{"b":1}}"#]);
        doc.toggle_row(0);
        doc.toggle_row(2); // unfold "a"
        assert_eq!(doc.rows_len(), 6);
        // Row 4 is the `}` that closes "a"; it folds it back up …
        doc.toggle_row(4);
        assert_eq!(doc.rows_len(), 4);
        // … and leaves the cursor on the row that stayed behind.
        assert_eq!(doc.cursor, 2);
    }

    #[test]
    fn folding_again_keeps_what_was_open_inside() {
        let mut doc = doc(&[r#"{"a":{"b":{"c":1}}}"#]);
        doc.toggle_row(0);
        doc.toggle_row(2);
        doc.toggle_row(3);
        let deep = doc.rows_len();

        doc.toggle_row(2);
        assert!(doc.rows_len() < deep);
        doc.toggle_row(2);
        assert_eq!(doc.rows_len(), deep);
    }

    #[test]
    fn collapsing_a_record_pulls_the_cursor_back_into_range() {
        let mut doc = doc(&[r#"{"a":1,"b":2}"#]);
        doc.toggle_row(0);
        doc.cursor = doc.rows_len() - 1;
        doc.toggle_row(0);
        assert_eq!(doc.rows_len(), 1);
        assert_eq!(doc.cursor, 0);
    }

    /// What the side pane draws: coloured, on one line, and the same whether or
    /// not the zoom has the record open.
    #[test]
    fn the_side_pane_line_is_coloured_and_ignores_the_fold_state() {
        let mut doc = doc(&[r#"{"a":1,"b":"x"}"#]);
        let flat = |d: &Doc| -> String {
            d.entries[0].line().iter().map(|(_, s)| s.clone()).collect()
        };
        assert_eq!(flat(&doc), r#"{"a": 1, "b": "x"}"#);

        let cells = doc.entries[0].line();
        let toks: Vec<JsonTok> = cells.iter().map(|(t, _)| *t).collect();
        assert!(toks.contains(&JsonTok::Key), "{toks:?}");
        assert!(toks.contains(&JsonTok::Num), "{toks:?}");
        assert!(toks.contains(&JsonTok::Str), "{toks:?}");
        // No fold marker: the pane has nothing to fold.
        assert!(!toks.contains(&JsonTok::Marker), "{toks:?}");

        // Unfolding it in the zoom leaves the pane's line alone.
        doc.toggle_row(0);
        assert_eq!(flat(&doc), r#"{"a": 1, "b": "x"}"#);
    }

    #[test]
    fn an_invalid_record_keeps_its_raw_text_in_the_side_pane() {
        let doc = doc(&["not json"]);
        let cells = doc.entries[0].line();
        assert_eq!(cells[0].0, JsonTok::Error);
        assert_eq!(cells[0].1, "not json");
    }

    #[test]
    fn an_invalid_record_reports_itself_rather_than_vanishing() {
        let mut doc = doc(&["not json"]);
        assert_eq!(doc.entries.len(), 1);
        assert!(rendered(&doc)[0].contains("not json"));
        doc.toggle_row(0);
        let rows = rendered(&doc);
        assert!(rows[1].starts_with("invalid JSON:"), "{rows:?}");
        assert_eq!(rows[2], "not json");
    }

    #[test]
    fn array_elements_carry_their_index() {
        let mut doc = doc(&[r#"[1,{"a":2}]"#]);
        doc.toggle_row(0);
        assert_eq!(rendered(&doc), ["▾ [2]", "[", "  1,", "▸ {\"a\": 2}", "]"]);
        assert_eq!(doc.rows()[3].toggle.as_deref(), Some(&[Seg::Index(1)][..]));

        doc.toggle_row(3);
        assert_eq!(
            rendered(&doc),
            ["▾ [2]", "[", "  1,", "▾ {", "    \"a\": 2", "  }", "]"]
        );
    }

    #[test]
    fn single_row_values_stay_on_one_row() {
        for json in ["42", r#""hi""#, "null", "{}", "[]"] {
            let mut doc = doc(&[json]);
            doc.toggle_row(0);
            assert_eq!(doc.rows_len(), 2, "{json}");
        }
    }

    #[test]
    fn a_folded_preview_is_cut_at_the_budget() {
        let long = "x".repeat(PREVIEW_BUDGET * 2);
        let doc = doc(&[&format!(r#"{{"k":"{long}"}}"#)]);
        let width: usize = doc.rows()[0]
            .cells
            .iter()
            .map(|(_, s)| s.chars().count())
            .sum();
        assert!(width < PREVIEW_BUDGET * 2, "{width} columns is the whole value");
    }

    /// `←` unwinds one level per press, and only gives way at the top.
    #[test]
    fn back_folds_its_way_out_before_giving_way() {
        let mut doc = doc(&[r#"{"a":{"b":{"c":1}}}"#]);
        doc.toggle_row(0); // expand the record
        doc.toggle_row(2); // unfold "a"
        doc.toggle_row(3); // unfold "b"
        assert_eq!(
            rendered(&doc),
            [
                "▾ {1}",
                "{",
                "▾ \"a\": {",
                "  ▾ \"b\": {",
                "      \"c\": 1",
                "    }",
                "  }",
                "}",
            ]
        );

        // From the scalar inside "b": out to "b", which then folds.
        doc.cursor = 4;
        assert!(doc.back());
        assert_eq!(doc.cursor, 3);
        assert!(doc.back());
        assert_eq!(doc.rows_len(), 6, "{:?}", rendered(&doc));

        // "b" is folded now, so the next step is out to "a", which folds too.
        assert!(doc.back());
        assert_eq!(doc.cursor, 2);
        assert!(doc.back());
        assert_eq!(doc.rows_len(), 4);

        // Out of "a" to the record's own row, then the record folds …
        assert!(doc.back());
        assert_eq!(doc.cursor, 0);
        assert!(doc.back());
        assert_eq!(doc.rows_len(), 1);

        // … and only now is there nothing left to close.
        assert!(!doc.back());
    }

    #[test]
    fn back_closes_a_block_from_its_closing_bracket_too() {
        let mut doc = doc(&[r#"{"a":{"b":1}}"#]);
        doc.toggle_row(0);
        doc.toggle_row(2);
        // Row 4 is the `}` closing "a".
        doc.cursor = 4;
        assert!(doc.back());
        assert_eq!(doc.rows_len(), 4);
        assert_eq!(doc.cursor, 2);
    }

    #[test]
    fn back_steps_out_of_a_folded_sibling_rather_than_opening_it() {
        let mut doc = doc(&[r#"{"a":{"b":{"c":1}}}"#]);
        doc.toggle_row(0);
        doc.toggle_row(2); // unfold "a"; row 3 is the folded "b"
        let before = doc.rows_len();
        doc.cursor = 3;
        assert!(doc.back());
        // "b" stays folded — ← never opens anything.
        assert_eq!(doc.rows_len(), before);
        assert_eq!(doc.cursor, 2);
    }

    #[test]
    fn back_on_a_folded_record_gives_way_at_once() {
        let mut doc = doc(&[r#"{"a":1}"#, r#"{"b":2}"#]);
        doc.cursor = 1;
        assert!(!doc.back());
        assert_eq!(doc.cursor, 1, "giving way moves nothing");
    }

    #[test]
    fn back_out_of_a_record_body_lands_on_its_own_row() {
        let mut doc = doc(&[r#"{"a":1}"#, r#"{"b":2}"#]);
        doc.toggle_row(1); // expand the second record
        doc.cursor = 3; // the `"b": 2` row inside it
        assert!(doc.back());
        assert_eq!(doc.cursor, 1, "the second record's own row");
    }

    // ── a whole JSON file ────────────────────────────────────────────────

    fn json(text: &str) -> JsonDoc {
        JsonDoc::new(serde_json::from_str(text).unwrap())
    }

    /// The same file shut back to its top level, which is where folding starts
    /// from once you have collapsed what the zoom opened for you.
    fn folded_json(text: &str) -> JsonDoc {
        let mut doc = json(text);
        doc.collapse_all();
        doc
    }

    fn json_rendered(doc: &JsonDoc) -> Vec<String> {
        doc.rows()
            .iter()
            .map(|r| r.cells.iter().map(|(_, s)| s.as_str()).collect())
            .collect()
    }

    /// What the zoom shows the moment it opens: the whole file, every container
    /// in it unfolded, however deep it goes.
    #[test]
    fn a_json_file_opens_all_of_itself_to_start_with() {
        let doc = json(r#"{"a":1,"b":{"c":2},"d":[1,2]}"#);
        assert_eq!(
            json_rendered(&doc),
            [
                "{",
                "  \"a\": 1,",
                "▾ \"b\": {",
                "    \"c\": 2",
                "  },",
                "▾ \"d\": [",
                "    1,",
                "    2",
                "  ]",
                "}",
            ]
        );
    }

    /// Folded back up, the file reads as its top level: the root's own members,
    /// with everything nested under them folded onto a row each.
    #[test]
    fn a_folded_json_file_reads_one_level_down() {
        let doc = folded_json(r#"{"a":1,"b":{"c":2},"d":[1,2]}"#);
        assert_eq!(
            json_rendered(&doc),
            [
                "{",
                "  \"a\": 1,",
                "▸ \"b\": {\"c\": 2},",
                "▸ \"d\": [1, 2]",
                "}",
            ]
        );
    }

    #[test]
    fn unfolding_a_member_reveals_its_own_level_only() {
        let mut doc = folded_json(r#"{"a":{"b":{"c":1}}}"#);
        doc.toggle_row(1); // "a"
        assert_eq!(
            json_rendered(&doc),
            ["{", "▾ \"a\": {", "  ▸ \"b\": {\"c\": 1}", "  }", "}"]
        );
    }

    /// The document's own brackets fold nothing — collapsing a file to `{…}`
    /// says nothing, and `←` there is better spent leaving the zoom.
    #[test]
    fn the_root_brackets_do_not_fold() {
        let mut doc = json(r#"{"a":1}"#);
        let before = doc.rows_len();
        assert!(doc.rows()[0].toggle.is_none());
        doc.toggle_row(0);
        assert_eq!(doc.rows_len(), before);
    }

    #[test]
    fn a_json_block_folds_from_its_closing_bracket_too() {
        let mut doc = folded_json(r#"{"a":{"b":1}}"#);
        doc.toggle_row(1); // unfold "a"
        assert_eq!(doc.rows_len(), 5);
        doc.toggle_row(3); // the `}` closing "a"
        assert_eq!(doc.rows_len(), 3);
        assert_eq!(doc.cursor, 1, "the cursor lands on the row that stayed");
    }

    #[test]
    fn folding_a_json_block_again_keeps_what_was_open_inside() {
        let mut doc = folded_json(r#"{"a":{"b":{"c":1}}}"#);
        doc.toggle_row(1);
        doc.toggle_row(2);
        let deep = doc.rows_len();

        doc.toggle_row(1);
        assert!(doc.rows_len() < deep);
        doc.toggle_row(1);
        assert_eq!(doc.rows_len(), deep);
    }

    /// `←` unwinds a level per press, and only gives way at the document's level.
    #[test]
    fn json_back_folds_its_way_out_before_giving_way() {
        let mut doc = folded_json(r#"{"a":{"b":{"c":1}}}"#);
        doc.toggle_row(1); // unfold "a"
        doc.toggle_row(2); // unfold "b"

        // From the scalar inside "b": out to "b", which then folds.
        doc.cursor = 3;
        assert!(doc.back());
        assert_eq!(doc.cursor, 2);
        assert!(doc.back());
        assert_eq!(doc.rows_len(), 5, "{:?}", json_rendered(&doc));

        // "b" is folded, so the next step is out to "a", which folds too.
        assert!(doc.back());
        assert_eq!(doc.cursor, 1);
        assert!(doc.back());
        assert_eq!(doc.rows_len(), 3);

        // Back at the document's own level there is nothing left to close.
        assert!(!doc.back());
    }

    #[test]
    fn json_back_steps_out_of_a_folded_sibling_rather_than_opening_it() {
        let mut doc = folded_json(r#"{"a":{"b":{"c":1}}}"#);
        doc.toggle_row(1); // unfold "a"; row 2 is the folded "b"
        let before = doc.rows_len();
        doc.cursor = 2;
        assert!(doc.back());
        assert_eq!(doc.rows_len(), before, "← never opens anything");
        assert_eq!(doc.cursor, 1);
    }

    #[test]
    fn an_empty_container_says_all_it_has_to_on_one_row() {
        for text in ["{}", "[]"] {
            let doc = json(text);
            assert_eq!(json_rendered(&doc), [text], "{text}");
            assert!(doc.rows()[0].toggle.is_none());
        }
    }

    #[test]
    fn an_array_root_opens_the_same_way() {
        let doc = folded_json(r#"[1,{"a":2}]"#);
        assert_eq!(
            json_rendered(&doc),
            ["[", "  1,", "▸ {\"a\": 2}", "]"]
        );
        assert_eq!(doc.rows()[2].toggle.as_deref(), Some(&[Seg::Index(1)][..]));
    }

    #[test]
    fn the_json_cursor_never_leaves_the_document() {
        let mut doc = json(r#"{"a":1,"b":2}"#);
        doc.move_cursor(-5);
        assert_eq!(doc.cursor, 0);
        doc.move_cursor(50);
        assert_eq!(doc.cursor, doc.rows_len() - 1);
        doc.select_edge(true);
        assert_eq!(doc.cursor, 0);
    }

    /// Folding above the cursor can shorten the document under it.
    #[test]
    fn folding_pulls_the_json_cursor_back_into_range() {
        let mut doc = folded_json(r#"{"a":{"b":1,"c":2,"d":3}}"#);
        doc.toggle_row(1);
        doc.cursor = doc.rows_len() - 1;
        doc.toggle_row(1);
        assert!(doc.cursor < doc.rows_len());
    }

    #[test]
    fn the_cursor_never_leaves_the_document() {
        let mut doc = doc(&[r#"{"a":1}"#, r#"{"a":2}"#]);
        doc.move_cursor(-5);
        assert_eq!(doc.cursor, 0);
        doc.move_cursor(50);
        assert_eq!(doc.cursor, 1);
        doc.select_edge(true);
        assert_eq!(doc.cursor, 0);
    }

    // ── all at once ──────────────────────────────────────────────────────

    /// Two records of the same shape, nested three levels deep.
    fn nested() -> Doc {
        doc(&[
            r#"{"a":1,"m":{"p":2,"q":{"r":3}}}"#,
            r#"{"a":4,"m":{"p":5,"q":{"r":6}}}"#,
        ])
    }

    /// How deep a whole JSON file is unfolded, its own members counting as 1.
    fn json_depth(doc: &JsonDoc) -> usize {
        1 + doc.open.depth()
    }

    /// The cursor starts on a folded record, so there is no level to copy and
    /// `a` means all of it.
    #[test]
    fn expand_all_on_something_folded_opens_all_of_it() {
        let mut doc = nested();
        assert_eq!(doc.expand_all(), None);
        assert_eq!(
            rendered(&doc),
            [
                "▾ {2}",
                "{",
                r#"  "a": 1,"#,
                r#"▾ "m": {"#,
                r#"    "p": 2,"#,
                r#"  ▾ "q": {"#,
                r#"      "r": 3"#,
                "    }",
                "  }",
                "}",
                "▾ {2}",
                "{",
                r#"  "a": 4,"#,
                r#"▾ "m": {"#,
                r#"    "p": 5,"#,
                r#"  ▾ "q": {"#,
                r#"      "r": 6"#,
                "    }",
                "  }",
                "}",
            ]
        );
    }

    /// A folded row inside an open record is still inside that record, so `a`
    /// there copies the record's level rather than opening the file whole.
    #[test]
    fn expand_all_on_a_folded_row_inside_a_record_copies_the_records_level() {
        let mut doc = nested();
        doc.toggle_row(0); // open the first record; row 3 is its folded "m"
        assert!(doc.rows()[3].folded);
        doc.set_cursor(3);

        assert_eq!(doc.expand_all(), Some(1));
        let rows = rendered(&doc);
        assert_eq!(
            rows.iter().filter(|r| r.contains("▸ \"m\": {")).count(),
            2,
            "both records open to their top level: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("▾ \"m\"")),
            "the level the cursor's record read at is not opened past: {rows:?}"
        );
    }

    /// And the same a level further in: what counts is how deep the record is
    /// open, not which of its rows the cursor happens to be resting on.
    #[test]
    fn a_folded_row_deeper_in_copies_the_deeper_level() {
        let mut doc = nested();
        doc.toggle_row(0); // the record
        doc.toggle_row(3); // its "m", leaving "q" folded inside
        let q = doc
            .rows()
            .iter()
            .position(|r| r.folded && r.cells.iter().any(|(_, t)| t.contains("\"q\"")))
            .expect("the folded \"q\" row");
        doc.set_cursor(q);

        assert_eq!(doc.expand_all(), Some(2));
        let rows = rendered(&doc);
        assert_eq!(
            rows.iter().filter(|r| r.contains("▾ \"m\": {")).count(),
            2,
            "{rows:?}"
        );
        assert_eq!(
            rows.iter().filter(|r| r.contains("▸ \"q\": {")).count(),
            2,
            "{rows:?}"
        );
    }

    /// Standing on something open, `a` means "the rest like this one".
    #[test]
    fn expand_all_levels_the_document_at_the_cursor() {
        let mut doc = nested();
        // Open the first record two levels: itself, then "m".
        doc.toggle_row(0);
        doc.toggle_row(3);
        assert_eq!(doc.record_depth(0), 2);

        assert_eq!(doc.expand_all(), Some(2));
        let rows = rendered(&doc);
        // Both records now show "m" open with "q" folded inside it.
        assert_eq!(rows.iter().filter(|r| r.contains("▾ \"m\": {")).count(), 2);
        assert_eq!(
            rows.iter().filter(|r| r.contains("▸ \"q\": {")).count(),
            2,
            "{rows:?}"
        );
    }

    /// Levelling off is not only opening: a record somebody had opened deeper
    /// than the cursor folds back, or "every record at level 1" would be a lie.
    #[test]
    fn expand_all_folds_what_is_deeper_than_the_level() {
        let mut doc = nested();
        doc.expand_to(1);
        doc.toggle_row(3); // open the first record's "m", a level past the rest

        // Stand on the second record's own row, open at level 1, and level off.
        let header = doc.rows().iter().position(|r| r.entry == 1).unwrap();
        doc.set_cursor(header);
        assert_eq!(doc.expand_all(), Some(1));
        let rows = rendered(&doc);
        assert!(
            rows.iter().all(|r| !r.contains("▾ \"m\"")),
            "nothing is left open past level 1: {rows:?}"
        );
    }

    /// The menu's lines, indented by the level they are listed at.
    fn menu(doc: &Doc) -> Vec<String> {
        doc.keys()
            .rows()
            .iter()
            .map(|r| format!("{}{}", "  ".repeat(r.depth()), r.key))
            .collect()
    }

    /// What the key menu moves: one record, the rest of the file left alone.
    #[test]
    fn only_the_record_being_read_takes_the_menus_shape() {
        let mut doc = nested();
        doc.fold_keys(&key_path(&doc, "m"), true);
        doc.open_entry_to_keys(1);

        assert_eq!(doc.record_depth(1), 2, "open down to the key the menu lists");
        assert_eq!(doc.record_depth(0), 0, "the other record is untouched");

        // The cursor lands on the record that moved, whatever it was on before.
        assert_eq!(doc.rows()[doc.cursor()].entry, 1);
        assert_eq!(doc.rows()[doc.cursor()].sub, 0);

        // Folding the key back up takes that record back with it.
        doc.fold_keys(&key_path(&doc, "m"), false);
        doc.open_entry_to_keys(1);
        assert_eq!(doc.record_depth(1), 1, "left at its own top level");

        // A record that isn't there is not worth a panic.
        doc.open_entry_to_keys(9);
    }

    /// The menu and a record keep the same *shape*, not the same depth: an
    /// array is no level of naming in the menu, so a key listed two deep there
    /// can sit three containers deep in the record.
    #[test]
    fn a_record_opens_to_match_the_menu_through_an_array() {
        let mut doc = doc(&[r#"{"spans":[{"name":"x","status":{"code":"OK"}}]}"#]);
        doc.fold_keys(&key_path(&doc, "spans"), true);
        doc.fold_keys(&key_path(&doc, "status"), true);
        doc.open_entry_to_keys(0);

        let rows = rendered(&doc);
        assert!(rows.iter().any(|r| r.contains(r#"▾ "spans": ["#)), "{rows:?}");
        assert!(
            rows.iter().any(|r| r.contains(r#"▾ "status": {"#)),
            "the array between them is opened along with the key: {rows:?}"
        );
        assert!(rows.iter().any(|r| r.contains(r#""code": "OK""#)), "{rows:?}");

        // A key the menu does not list the contents of stays folded, however
        // deep the ones beside it are opened.
        doc.fold_keys(&key_path(&doc, "status"), false);
        doc.open_entry_to_keys(0);
        let rows = rendered(&doc);
        assert!(rows.iter().any(|r| r.contains(r#"▸ "status": {"#)), "{rows:?}");
    }

    #[test]
    fn the_menu_opens_to_match_the_record_it_is_opened_over() {
        let mut doc = doc(&[r#"{"spans":[{"name":"x","status":{"code":"OK"}}]}"#]);
        assert_eq!(menu(&doc), ["spans"], "folded: the record's own keys");

        // The record open all the way through the array, as `a` would leave it.
        doc.expand_to(4);
        doc.open_keys_to(0);
        assert_eq!(menu(&doc), ["spans", "  name", "  status", "    code"]);

        // And folded back to its top level, the menu comes back with it.
        doc.expand_to(1);
        doc.open_keys_to(0);
        assert_eq!(menu(&doc), ["spans"]);
    }

    #[test]
    fn collapse_all_folds_every_record_however_deep_it_was() {
        let mut doc = nested();
        doc.expand_to(3);
        assert!(doc.rows_len() > 2);

        doc.collapse_all();
        assert_eq!(
            rendered(&doc),
            [
                r#"▸ {"a": 1, "m": {"p": 2, "q": {"r": 3}}}"#,
                r#"▸ {"a": 4, "m": {"p": 5, "q": {"r": 6}}}"#,
            ]
        );
        // And it is a reset, not a fold: unfolding a record again starts at its
        // top level rather than reopening what used to be there.
        doc.toggle_row(0);
        assert_eq!(doc.record_depth(0), 1);
    }

    /// Both of these move every row in the document. The record you were reading
    /// is what the cursor holds on to.
    #[test]
    fn all_at_once_keeps_the_cursor_on_its_record() {
        let mut doc = nested();
        doc.expand_to(1);
        doc.set_cursor(7); // a body row of the second record
        assert_eq!(doc.rows()[doc.cursor()].entry, 1);

        doc.expand_all();
        assert_eq!(doc.rows()[doc.cursor()].entry, 1);
        assert_eq!(doc.rows()[doc.cursor()].sub, 0, "on the record's own row");

        doc.collapse_all();
        assert_eq!(doc.cursor(), 1);
        assert!(doc.cursor() < doc.rows_len());
    }

    #[test]
    fn a_record_that_does_not_parse_has_no_levels_to_speak_of() {
        let mut doc = doc(&[r#"{"a":{"b":1}}"#, "oops"]);
        doc.expand_to(3);
        let rows = rendered(&doc);
        assert!(rows.iter().any(|r| r.contains("invalid JSON")), "{rows:?}");
        doc.collapse_all();
        assert_eq!(doc.rows_len(), 2);
    }

    // ── all at once, over a whole JSON file ──────────────────────────────

    /// A file is one value, so there is nothing to level it against: `a` opens
    /// all of it wherever the cursor happens to be.
    #[test]
    fn a_json_file_opens_all_of_itself() {
        let mut doc = folded_json(r#"{"a":{"b":{"c":1}},"d":{"e":2}}"#);
        assert_eq!(json_depth(&doc), 1, "folded up, it reads one level down");

        assert_eq!(doc.expand_all(), None);
        assert_eq!(
            json_rendered(&doc),
            [
                "{",
                r#"▾ "a": {"#,
                r#"  ▾ "b": {"#,
                r#"      "c": 1"#,
                "    }",
                "  },",
                r#"▾ "d": {"#,
                r#"    "e": 2"#,
                "  }",
                "}",
            ]
        );

        // And `c` shuts it back to level 1 — the root brackets don't fold, so
        // that is as far as folding goes.
        doc.collapse_all();
        assert_eq!(
            json_rendered(&doc),
            ["{", r#"▸ "a": {"b": {"c": 1}},"#, r#"▸ "d": {"e": 2}"#, "}"]
        );
        assert_eq!(json_depth(&doc), 1);
    }

    // ── the key filter ───────────────────────────────────────────────────

    /// The path of the menu row for `key`, as far as the menu is unfolded.
    fn key_path(doc: &Doc, key: &str) -> Vec<usize> {
        doc.keys()
            .rows()
            .into_iter()
            .find(|r| r.key == key)
            .unwrap_or_else(|| panic!("no menu row for {key}"))
            .path
    }

    #[test]
    fn the_key_structure_is_learnt_from_the_records() {
        let doc = doc(&[r#"{"a":1}"#, r#"{"b":2}"#]);
        let keys: Vec<String> = doc.keys().rows().into_iter().map(|r| r.key).collect();
        assert_eq!(keys, ["a", "b"]);
    }

    #[test]
    fn a_switched_off_key_is_gone_from_every_row_it_had() {
        let mut doc = doc(&[r#"{"a":1,"b":2}"#]);
        doc.toggle_row(0); // unfold the record
        assert_eq!(
            rendered(&doc),
            [r#"▾ {2}"#, "{", r#"  "a": 1,"#, r#"  "b": 2"#, "}"]
        );

        let path = key_path(&doc, "b");
        doc.edit_keys(|f| f.toggle(&path));
        assert_eq!(rendered(&doc), [r#"▾ {1}"#, "{", r#"  "a": 1"#, "}"]);
        // And from the line the side pane draws.
        assert_eq!(line(&doc.entries[0]), r#"{"a": 1}"#);
    }

    /// The record is not re-read to filter it, so switching a key back on has
    /// to bring the value back exactly as it was.
    #[test]
    fn switching_a_key_back_on_restores_it() {
        let mut doc = doc(&[r#"{"a":1,"b":{"c":2}}"#]);
        let path = key_path(&doc, "b");
        doc.edit_keys(|f| f.toggle(&path));
        assert_eq!(line(&doc.entries[0]), r#"{"a": 1}"#);
        doc.edit_keys(|f| f.toggle(&path));
        assert_eq!(line(&doc.entries[0]), r#"{"a": 1, "b": {"c": 2}}"#);
    }

    #[test]
    fn a_record_that_does_not_parse_is_left_alone_by_the_filter() {
        let mut doc = doc(&[r#"{"a":1}"#, "oops"]);
        let path = key_path(&doc, "a");
        doc.edit_keys(|f| f.toggle(&path));
        assert!(line(&doc.entries[1]).starts_with("oops"));
    }

    /// Hiding a key shortens the record it was in, and the cursor has to come
    /// back with it rather than point past the end.
    #[test]
    fn filtering_pulls_the_cursor_back_into_range() {
        let mut doc = doc(&[r#"{"a":1,"b":2,"c":3}"#]);
        doc.toggle_row(0);
        doc.cursor = doc.rows_len() - 1;
        let path = key_path(&doc, "b");
        doc.edit_keys(|f| f.toggle(&path));
        assert!(doc.cursor < doc.rows_len());
    }

    /// Folding the menu is a menu affair; it must not disturb the records.
    #[test]
    fn folding_the_menu_shows_nothing_new_and_hides_nothing() {
        let mut doc = doc(&[r#"{"a":{"b":1}}"#]);
        let before = rendered(&doc);
        let path = key_path(&doc, "a");
        assert!(doc.fold_keys(&path, true));
        assert!(!doc.fold_keys(&path, true), "already unfolded");
        assert_eq!(rendered(&doc), before);
        assert_eq!(doc.keys().hidden(), 0);
    }
}
