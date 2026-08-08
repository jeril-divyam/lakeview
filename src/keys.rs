//! The key filter: the object keys a JSONL preview's records use, as a tree
//! whose nodes can be switched off. Whatever is off is pruned out of a record's
//! value before anything renders it, so one prune covers every way a key could
//! be shown — the side pane's lines, the folded previews and the unfolded
//! bodies alike.
//!
//! Wide records are mostly noise more often than not, and a `.jsonl` file is one
//! shape repeated, so switching a key off here is worth far more than it would
//! be over a single JSON document — which is why only [`crate::jsonl::Doc`]
//! carries one.

use serde_json::Value;

/// Records read to work out the key structure. JSONL is usually homogeneous, so
/// the first few hundred describe the whole file; a key that turns up only later
/// is never hidden, which is the safe way to be wrong. The preview is capped at
/// `preview_bytes` and parsed by the time this runs, so the cap bounds the walk
/// rather than any reading.
const SAMPLE: usize = 500;

/// One key of the structure, and the keys nested under it. Array elements are
/// not levels of their own: the keys inside `"items": [{...}]` sit directly
/// under `items`, which is how you would want to name them.
#[derive(Clone, Debug)]
struct Node {
    key: String,
    /// Whether records still show this key.
    enabled: bool,
    /// Whether the menu lists this node's children. The menu starts one level
    /// deep, like the zoom itself.
    open: bool,
    /// Whether this node or anything under it is switched off. Lets `prune` skip
    /// whole untouched subtrees.
    pruned: bool,
    children: Vec<Node>,
}

impl Node {
    fn new(key: String) -> Self {
        Self {
            key,
            enabled: true,
            open: false,
            pruned: false,
            children: Vec::new(),
        }
    }
}

/// One line of the filter menu: a flattened view of the tree, skipping the
/// children of nodes the menu has folded.
pub struct MenuRow {
    /// Child indices from the root down to this node, for addressing it.
    pub path: Vec<usize>,
    pub key: String,
    pub enabled: bool,
    /// Whether the menu is showing this node's children.
    pub open: bool,
    pub has_children: bool,
    /// On, but hiding something below it.
    pub partial: bool,
}

impl MenuRow {
    pub fn depth(&self) -> usize {
        self.path.len() - 1
    }
}

#[derive(Clone, Debug, Default)]
pub struct KeyFilter {
    roots: Vec<Node>,
    /// How many keys are switched off, cached so drawing never walks the tree.
    hidden: usize,
}

impl KeyFilter {
    /// Learn the key structure from the records at hand.
    pub fn discover<'a>(values: impl IntoIterator<Item = &'a Value>) -> Self {
        let mut roots = Vec::new();
        for value in values.into_iter().take(SAMPLE) {
            learn(&mut roots, value);
        }
        Self { roots, hidden: 0 }
    }

    /// True when the records have no object keys to filter at all.
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn hidden(&self) -> usize {
        self.hidden
    }

    /// Drop the switched-off keys from a value.
    pub fn prune(&self, value: &mut Value) {
        prune(value, &self.roots);
    }

    /// The menu's lines, top to bottom.
    pub fn rows(&self) -> Vec<MenuRow> {
        let mut rows = Vec::new();
        collect(&self.roots, &mut Vec::new(), &mut rows);
        rows
    }

    /// Switch a key, and everything nested under it, on or off. A key is shown
    /// only when its ancestors are too, so switching one off takes its whole
    /// subtree with it, and switching one on clears the way back down to it —
    /// that way a switch always says what the records will actually show.
    pub fn toggle(&mut self, path: &[usize]) {
        let Some(node) = self.node_mut(path) else {
            return;
        };
        let on = !node.enabled;
        node.enabled = on;
        set_all(&mut node.children, on);
        if on {
            let mut nodes = &mut self.roots;
            for &i in &path[..path.len() - 1] {
                let Some(node) = nodes.get_mut(i) else { break };
                node.enabled = true;
                nodes = &mut node.children;
            }
        }
        self.recount();
    }

    /// Whether the menu is listing the keys under `path` — the key names from a
    /// record's root down, array indices left out, an array being no level of
    /// naming here.
    pub fn is_open<S: AsRef<str>>(&self, path: &[S]) -> bool {
        let mut nodes = &self.roots;
        let mut open = false;
        for key in path {
            let Some(node) = nodes.iter().find(|n| n.key == key.as_ref()) else {
                return false;
            };
            open = node.open;
            nodes = &node.children;
        }
        open
    }

    /// Unfold exactly the keys `paths` names and fold every other, so the menu
    /// takes the shape of the record it was opened over. A key on the way down
    /// to one of them opens with it: the menu cannot list a key whose parent it
    /// isn't listing.
    pub fn open_only(&mut self, paths: &[Vec<String>]) {
        fold_all(&mut self.roots);
        for path in paths {
            let mut nodes = &mut self.roots;
            for key in path {
                let Some(at) = nodes.iter().position(|n| n.key == *key) else {
                    break;
                };
                let node = &mut nodes[at];
                // A key with nothing under it lists nothing, marked or not.
                node.open = !node.children.is_empty();
                nodes = &mut node.children;
            }
        }
    }

    /// Unfold or fold a node in the menu, reporting whether anything moved.
    pub fn set_open(&mut self, path: &[usize], open: bool) -> bool {
        match self.node_mut(path) {
            Some(node) if !node.children.is_empty() && node.open != open => {
                node.open = open;
                true
            }
            _ => false,
        }
    }

    pub fn set_all(&mut self, enabled: bool) {
        set_all(&mut self.roots, enabled);
        self.recount();
    }

    fn node_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        let (last, ancestors) = path.split_last()?;
        let mut nodes = &mut self.roots;
        for &i in ancestors {
            nodes = &mut nodes.get_mut(i)?.children;
        }
        nodes.get_mut(*last)
    }

    fn recount(&mut self) {
        self.hidden = recount(&mut self.roots);
    }
}

/// Fold every object key `value` contains into `nodes`, in first-seen order.
fn learn(nodes: &mut Vec<Node>, value: &Value) {
    match value {
        Value::Object(map) => {
            for (key, val) in map {
                let at = match nodes.iter().position(|n| n.key == *key) {
                    Some(i) => i,
                    None => {
                        nodes.push(Node::new(key.clone()));
                        nodes.len() - 1
                    }
                };
                learn(&mut nodes[at].children, val);
            }
        }
        // An array is a repetition of one shape, not a level of naming.
        Value::Array(items) => items.iter().for_each(|item| learn(nodes, item)),
        _ => {}
    }
}

fn find<'a>(nodes: &'a [Node], key: &str) -> Option<&'a Node> {
    nodes.iter().find(|n| n.key == key)
}

fn prune(value: &mut Value, nodes: &[Node]) {
    // Nothing below here is switched off, so the rest of the subtree is as it
    // should be already.
    if !nodes.iter().any(|n| n.pruned) {
        return;
    }
    match value {
        Value::Object(map) => {
            // A key the sample never saw has no node, and stays.
            map.retain(|key, _| find(nodes, key).is_none_or(|n| n.enabled));
            for (key, val) in map.iter_mut() {
                if let Some(node) = find(nodes, key).filter(|n| n.pruned) {
                    prune(val, &node.children);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| prune(item, nodes)),
        _ => {}
    }
}

fn collect(nodes: &[Node], path: &mut Vec<usize>, rows: &mut Vec<MenuRow>) {
    for (i, node) in nodes.iter().enumerate() {
        path.push(i);
        rows.push(MenuRow {
            path: path.clone(),
            key: node.key.clone(),
            enabled: node.enabled,
            open: node.open,
            has_children: !node.children.is_empty(),
            partial: node.enabled && node.pruned,
        });
        if node.open {
            collect(&node.children, path, rows);
        }
        path.pop();
    }
}

fn fold_all(nodes: &mut [Node]) {
    for node in nodes.iter_mut() {
        node.open = false;
        fold_all(&mut node.children);
    }
}

fn set_all(nodes: &mut [Node], enabled: bool) {
    for node in nodes {
        node.enabled = enabled;
        set_all(&mut node.children, enabled);
    }
}

/// Refresh the `pruned` flags bottom-up, returning how many keys are off.
fn recount(nodes: &mut [Node]) -> usize {
    let mut hidden = 0;
    for node in nodes.iter_mut() {
        let below = recount(&mut node.children);
        node.pruned = !node.enabled || below > 0;
        hidden += below + usize::from(!node.enabled);
    }
    hidden
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Discovery works off parsed records, so a line that doesn't parse
    /// contributes no keys — which is exactly what the viewer does with it.
    fn filter(lines: &[&str]) -> KeyFilter {
        let values: Vec<Value> = lines
            .iter()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        KeyFilter::discover(values.iter())
    }

    fn value(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    fn keys(filter: &KeyFilter) -> Vec<String> {
        filter
            .rows()
            .iter()
            .map(|r| format!("{}{}", "  ".repeat(r.depth()), r.key))
            .collect()
    }

    /// The path of the row whose key is `key`, as far as the menu is unfolded.
    fn path(filter: &KeyFilter, key: &str) -> Vec<usize> {
        filter
            .rows()
            .into_iter()
            .find(|r| r.key == key)
            .unwrap_or_else(|| panic!("no row for {key}"))
            .path
    }

    fn pruned(filter: &KeyFilter, json: &str) -> String {
        let mut v = value(json);
        filter.prune(&mut v);
        serde_json::to_string(&v).unwrap()
    }

    #[test]
    fn discovery_unions_the_keys_of_every_record() {
        let f = filter(&[r#"{"a":1,"b":2}"#, r#"{"b":3,"c":4}"#]);
        assert_eq!(keys(&f), ["a", "b", "c"]);
    }

    #[test]
    fn the_menu_starts_one_level_deep() {
        let f = filter(&[r#"{"a":{"b":{"c":1}},"d":2}"#]);
        assert_eq!(keys(&f), ["a", "d"]);
    }

    #[test]
    fn unfolding_shows_the_next_level_only() {
        let mut f = filter(&[r#"{"a":{"b":{"c":1}},"d":2}"#]);
        assert!(f.set_open(&path(&f, "a"), true));
        assert_eq!(keys(&f), ["a", "  b", "d"]);
        assert!(f.set_open(&path(&f, "b"), true));
        assert_eq!(keys(&f), ["a", "  b", "    c", "d"]);

        // Folding it again is a no-op the second time around.
        assert!(f.set_open(&path(&f, "a"), false));
        assert!(!f.set_open(&path(&f, "a"), false));
        assert_eq!(keys(&f), ["a", "d"]);
    }

    /// What the records are folded to match: the keys the menu is listing the
    /// contents of.
    #[test]
    fn a_path_is_open_when_the_menu_lists_the_keys_under_it() {
        let mut f = filter(&[r#"{"a":{"b":{"c":1}},"d":2}"#]);
        assert!(!f.is_open(&["a"]));

        f.set_open(&path(&f, "a"), true);
        assert!(f.is_open(&["a"]));
        assert!(!f.is_open(&["a", "b"]), "b is listed, not unfolded");

        // Neither a key that isn't there nor the root itself is a key that is
        // open — a record has to be asked about something.
        assert!(!f.is_open(&["a", "zzz"]));
        assert!(!f.is_open(&["zzz"]));
        assert!(!f.is_open::<&str>(&[]));
    }

    /// And the menu folded to match a record.
    #[test]
    fn open_only_folds_the_menu_to_the_paths_it_is_given() {
        let mut f = filter(&[r#"{"a":{"b":{"c":1}},"d":{"e":2}}"#]);
        f.open_only(&[vec!["a".into(), "b".into()]]);
        assert_eq!(keys(&f), ["a", "  b", "    c", "d"]);

        // Whatever was open and is not named this time folds back up.
        f.open_only(&[vec!["d".into()]]);
        assert_eq!(keys(&f), ["a", "d", "  e"]);

        f.open_only(&[]);
        assert_eq!(keys(&f), ["a", "d"]);
    }

    #[test]
    fn open_only_passes_over_a_key_that_is_not_there() {
        let mut f = filter(&[r#"{"a":1,"b":{"c":2}}"#]);
        f.open_only(&[vec!["zzz".into()], vec!["a".into()], vec!["b".into()]]);
        // "a" has nothing under it to list, marked or not.
        assert_eq!(keys(&f), ["a", "b", "  c"]);
    }

    #[test]
    fn keys_inside_arrays_are_not_a_level_of_their_own() {
        let mut f = filter(&[r#"{"items":[{"id":1},{"name":"x"}]}"#]);
        f.set_open(&path(&f, "items"), true);
        assert_eq!(keys(&f), ["items", "  id", "  name"]);
        f.toggle(&path(&f, "id"));
        assert_eq!(
            pruned(&f, r#"{"items":[{"id":1,"name":"x"},{"id":2}]}"#),
            r#"{"items":[{"name":"x"},{}]}"#
        );
    }

    #[test]
    fn switching_a_key_off_prunes_it_everywhere() {
        let mut f = filter(&[r#"{"a":1,"b":2}"#]);
        assert_eq!(f.hidden(), 0);
        f.toggle(&path(&f, "b"));
        assert_eq!(f.hidden(), 1);
        assert_eq!(pruned(&f, r#"{"a":1,"b":2}"#), r#"{"a":1}"#);
        // And on a record the sample never saw.
        assert_eq!(pruned(&f, r#"{"b":9,"z":0}"#), r#"{"z":0}"#);

        f.toggle(&path(&f, "b"));
        assert_eq!(f.hidden(), 0);
        assert_eq!(pruned(&f, r#"{"a":1,"b":2}"#), r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn a_nested_key_is_pruned_without_touching_its_parent() {
        let mut f = filter(&[r#"{"a":{"b":1,"c":2}}"#]);
        f.set_open(&path(&f, "a"), true);
        f.toggle(&path(&f, "b"));
        assert_eq!(pruned(&f, r#"{"a":{"b":1,"c":2}}"#), r#"{"a":{"c":2}}"#);

        // The parent reports that it is hiding something below it.
        let rows = f.rows();
        assert!(rows[0].partial && rows[0].enabled);
        assert!(!rows[1].enabled);
    }

    /// The switches every menu row would show, deepest levels unfolded.
    fn switches(f: &mut KeyFilter) -> Vec<String> {
        // Unfolding cannot invalidate the rows above it, so one pass down the
        // list reaches every level.
        let mut i = 0;
        while i < f.rows().len() {
            let path = f.rows()[i].path.clone();
            f.set_open(&path, true);
            i += 1;
        }
        f.rows()
            .iter()
            .map(|r| {
                format!(
                    "{}{} {}",
                    "  ".repeat(r.depth()),
                    match (r.enabled, r.partial) {
                        (false, _) => "[ ]",
                        (true, true) => "[~]",
                        (true, false) => "[x]",
                    },
                    r.key
                )
            })
            .collect()
    }

    #[test]
    fn switching_a_key_off_takes_everything_under_it_with_it() {
        let mut f = filter(&[r#"{"status":{"code":1,"msg":{"text":"x"}},"kind":"a"}"#]);
        f.set_open(&path(&f, "status"), true);
        f.toggle(&path(&f, "status"));
        assert_eq!(
            switches(&mut f),
            [
                "[ ] status",
                "  [ ] code",
                "  [ ] msg",
                "    [ ] text",
                "[x] kind",
            ],
            "a switched-off subtree must not still claim to be shown"
        );
        assert_eq!(f.hidden(), 4);
        assert_eq!(
            pruned(&f, r#"{"status":{"code":1,"msg":{"text":"x"}},"kind":"a"}"#),
            r#"{"kind":"a"}"#
        );

        // And switching it back on brings the subtree back with it.
        f.toggle(&path(&f, "status"));
        assert_eq!(f.hidden(), 0);
    }

    #[test]
    fn switching_a_key_on_clears_the_way_down_to_it() {
        let mut f = filter(&[r#"{"a":{"b":{"c":1,"d":2}}}"#]);
        f.set_open(&path(&f, "a"), true);
        f.toggle(&path(&f, "a"));
        assert_eq!(f.hidden(), 4);

        // Switching one leaf back on has to reopen its ancestors, or a `[x]`
        // would promise a key that its switched-off parents still hide.
        switches(&mut f); // unfold the menu far enough to reach the leaf
        f.toggle(&path(&f, "c"));
        assert_eq!(
            switches(&mut f),
            ["[~] a", "  [~] b", "    [x] c", "    [ ] d"]
        );
        assert_eq!(
            pruned(&f, r#"{"a":{"b":{"c":1,"d":2}}}"#),
            r#"{"a":{"b":{"c":1}}}"#
        );
    }

    #[test]
    fn a_key_with_the_same_name_elsewhere_is_left_alone() {
        let mut f = filter(&[r#"{"a":{"id":1},"b":{"id":2}}"#]);
        f.set_open(&path(&f, "a"), true);
        f.toggle(&path(&f, "id"));
        assert_eq!(
            pruned(&f, r#"{"a":{"id":1},"b":{"id":2}}"#),
            r#"{"a":{},"b":{"id":2}}"#
        );
    }

    #[test]
    fn set_all_reaches_folded_levels() {
        let mut f = filter(&[r#"{"a":{"b":1},"c":2}"#]);
        f.set_all(false);
        assert_eq!(f.hidden(), 3);
        assert_eq!(pruned(&f, r#"{"a":{"b":1},"c":2}"#), "{}");
        f.set_all(true);
        assert_eq!(f.hidden(), 0);
    }

    #[test]
    fn records_without_object_keys_have_nothing_to_filter() {
        let f = filter(&["42", r#""hi""#, "[1,2]", "oops"]);
        assert!(f.is_empty());
        assert!(f.rows().is_empty());
        assert_eq!(pruned(&f, "42"), "42");
    }

    #[test]
    fn only_the_first_records_are_sampled() {
        let lines: Vec<String> = (0..SAMPLE + 10)
            .map(|i| format!(r#"{{"k{i}":1}}"#))
            .collect();
        let f = filter(&lines.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(f.rows().len(), SAMPLE);
    }

    #[test]
    fn addressing_a_key_that_is_gone_is_harmless() {
        let mut f = filter(&[r#"{"a":1}"#]);
        f.toggle(&[7]);
        f.toggle(&[]);
        assert!(!f.set_open(&[0, 3], true));
        assert_eq!(f.hidden(), 0);
    }
}
