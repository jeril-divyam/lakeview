//! The JSONL preview: one folded row per record, unfolding a level at a time.
//!
//! A preview is capped at `preview_bytes`, so unlike the standalone viewer this
//! is ported from, every record is parsed up front and nothing needs a cache.
//!
//! Rows are built in the same `(JsonTok, String)` vocabulary the whole-file JSON
//! preview uses, so the UI colours both through `Theme::json` and this module
//! stays free of ratatui.

use std::collections::HashMap;

use serde_json::Value;

use crate::app::JsonTok;

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
#[derive(Default, Debug)]
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

// ── the document ─────────────────────────────────────────────────────────

pub struct Entry {
    /// The record's text, verbatim — what the side pane shows.
    pub raw: String,
    value: Option<Value>,
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
            error,
            expanded: false,
            open: Open::default(),
        }
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

        match (&self.value, &self.error) {
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

    fn body(&self) -> Vec<Row> {
        if let Some(err) = &self.error {
            return vec![
                Row::plain(vec![(JsonTok::Error, format!("invalid JSON: {err}"))]),
                Row::plain(vec![(JsonTok::Error, self.raw.clone())]),
            ];
        }
        let mut out = Vec::new();
        if let Some(value) = &self.value {
            write_body(&mut out, value, &self.open);
        }
        out
    }
}

pub struct Doc {
    pub entries: Vec<Entry>,
    /// Selected row, indexing `rows()`.
    pub cursor: usize,
    /// The fetch stopped at `preview_bytes`, so this is not the whole file.
    pub truncated: bool,
}

/// Split `text` into records. A capped fetch usually stops mid-record, so the
/// fragment is dropped rather than reported as a parse error the file does not
/// actually have.
pub fn parse(text: &str, truncated: bool) -> Doc {
    let mut lines: Vec<&str> = text.lines().collect();
    if truncated && !text.ends_with('\n') {
        lines.pop();
    }
    let entries = lines
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Entry::new(line.to_string()))
        .collect();
    Doc {
        entries,
        cursor: 0,
        truncated,
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

    pub fn len(&self) -> usize {
        self.rows().len()
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let last = self.len().saturating_sub(1) as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
    }

    pub fn select_edge(&mut self, first: bool) {
        self.cursor = if first {
            0
        } else {
            self.len().saturating_sub(1)
        };
    }

    /// Unfold or fold whatever `row` heads, and select it. A record's own row
    /// folds the whole record; a body row folds the container it names, one
    /// level at a time.
    pub fn toggle_row(&mut self, row: usize) {
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
        self.cursor = self.cursor.min(self.len().saturating_sub(1));
    }

    pub fn toggle_cursor(&mut self) {
        self.toggle_row(self.cursor);
    }

    /// `←` — ascend, the way it does in the tree: fold what is open, else step
    /// out to whatever encloses the selection. Returns `false` once there is
    /// nothing left to close, which is the caller's cue to leave the zoom.
    pub fn back(&mut self) -> bool {
        let rows = self.rows();
        let Some(row) = rows.get(self.cursor) else {
            return false;
        };
        let entry = row.entry;

        // The record itself: fold it up, or — already folded — give way.
        if row.sub == 0 {
            if !self.entries[entry].expanded {
                return false;
            }
            self.entries[entry].expanded = false;
            self.cursor = self.cursor.min(self.len().saturating_sub(1));
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

    #[test]
    fn the_closing_bracket_folds_the_same_container() {
        let mut doc = doc(&[r#"{"a":{"b":1}}"#]);
        doc.toggle_row(0);
        doc.toggle_row(2); // unfold "a"
        assert_eq!(doc.len(), 6);
        // Row 4 is the `}` that closes "a"; it folds it back up …
        doc.toggle_row(4);
        assert_eq!(doc.len(), 4);
        // … and leaves the cursor on the row that stayed behind.
        assert_eq!(doc.cursor, 2);
    }

    #[test]
    fn folding_again_keeps_what_was_open_inside() {
        let mut doc = doc(&[r#"{"a":{"b":{"c":1}}}"#]);
        doc.toggle_row(0);
        doc.toggle_row(2);
        doc.toggle_row(3);
        let deep = doc.len();

        doc.toggle_row(2);
        assert!(doc.len() < deep);
        doc.toggle_row(2);
        assert_eq!(doc.len(), deep);
    }

    #[test]
    fn collapsing_a_record_pulls_the_cursor_back_into_range() {
        let mut doc = doc(&[r#"{"a":1,"b":2}"#]);
        doc.toggle_row(0);
        doc.cursor = doc.len() - 1;
        doc.toggle_row(0);
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.cursor, 0);
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
            assert_eq!(doc.len(), 2, "{json}");
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
        assert_eq!(doc.len(), 6, "{:?}", rendered(&doc));

        // "b" is folded now, so the next step is out to "a", which folds too.
        assert!(doc.back());
        assert_eq!(doc.cursor, 2);
        assert!(doc.back());
        assert_eq!(doc.len(), 4);

        // Out of "a" to the record's own row, then the record folds …
        assert!(doc.back());
        assert_eq!(doc.cursor, 0);
        assert!(doc.back());
        assert_eq!(doc.len(), 1);

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
        assert_eq!(doc.len(), 4);
        assert_eq!(doc.cursor, 2);
    }

    #[test]
    fn back_steps_out_of_a_folded_sibling_rather_than_opening_it() {
        let mut doc = doc(&[r#"{"a":{"b":{"c":1}}}"#]);
        doc.toggle_row(0);
        doc.toggle_row(2); // unfold "a"; row 3 is the folded "b"
        let before = doc.len();
        doc.cursor = 3;
        assert!(doc.back());
        // "b" stays folded — ← never opens anything.
        assert_eq!(doc.len(), before);
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
}
