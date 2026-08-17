//! The pane state: repositories and their refs, the object tree's arena,
//! the commit list and the preview — the structs the renderer reads and the
//! update loop in `mod.rs` drives.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use ratatui::widgets::ListState;

use crate::lakefs::{Commit, NamedRef, ObjectStats, RefKind, Repository};

// ── shared bits ──────────────────────────────────────────────────────────

pub enum Load {
    /// Not requested yet — a collapsed directory that has never been opened.
    Idle,
    Loading,
    Ready,
    Failed(String),
}

/// What a pane-one row stands for. The tree pane renders straight from its
/// `Node`s, so directories and files need no variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Repo,
    Branch,
    Tag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Repos,
    Tree,
    /// The preview, reading a `.json`. Unlike the other two this is not a list
    /// of rows to select but a document to fold, and `→` only steps into it when
    /// there is one — a `.jsonl`, a text file or a hex dump has nothing here for
    /// the cursor to do, and stays the tree's to scroll.
    Preview,
}

pub(super) fn move_in(state: &mut ListState, len: usize, delta: isize) {
    if len == 0 {
        state.select(None);
        return;
    }
    let current = state.selected().unwrap_or(0) as isize;
    let next = (current + delta).clamp(0, len as isize - 1) as usize;
    state.select(Some(next));
}

// ── pane 1: repositories, expandable to their refs ───────────────────────

pub struct RefsSlot {
    pub load: Load,
    pub refs: Vec<NamedRef>,
    pub req: u64,
}

impl RefsSlot {
    /// Refs worth listing under the repository. A repository with a single
    /// branch leaves it out: the repository row already stands for its default
    /// branch, so a row of its own would only add a step. Tags are always
    /// listed, and a lone branch that isn't the recognised default stays —
    /// nothing else would reach it.
    fn visible(&self) -> impl Iterator<Item = &NamedRef> {
        let lone = self
            .refs
            .iter()
            .filter(|r| r.kind == RefKind::Branch)
            .count()
            == 1;
        self.refs
            .iter()
            .filter(move |r| !(lone && r.kind == RefKind::Branch && r.is_default))
    }

    /// Whether expanding this repository would reveal anything at all.
    pub(super) fn has_visible(&self) -> bool {
        self.visible().next().is_some()
    }
}

/// A render-ready line, cached so filtering isn't redone every frame.
pub struct ReposRow {
    pub label: String,
    pub meta: String,
    pub kind: RowKind,
    /// Highlights the repository's default branch.
    pub primary: bool,
    pub repo: String,
    /// `Some` on ref rows, `None` on repository rows.
    pub reference: Option<String>,
    pub expanded: bool,
    /// Whether expanding this row shows anything. Optimistic until the refs
    /// land — a repository is assumed to have some to list.
    pub expandable: bool,
    /// A repository row whose refs are still in flight.
    pub loading: bool,
}

#[derive(Default)]
pub struct ReposView {
    pub repos: Vec<Repository>,
    pub refs: HashMap<String, RefsSlot>,
    /// Whether each repository has refs worth listing, as answered by the
    /// start-up probe. Only consulted until the full refs arrive.
    pub probe: HashMap<String, bool>,
    pub expanded: HashSet<String>,
    pub rows: Vec<ReposRow>,
    pub state: ListState,
    pub filter: String,
    pub req: u64,
}

impl ReposView {
    pub fn selected_row(&self) -> Option<&ReposRow> {
        self.state.selected().and_then(|i| self.rows.get(i))
    }

    /// The (repo, ref) pair the current row points at. A ref row names its own
    /// ref; a repository row stands in for its default branch, which the
    /// repository listing already carries.
    pub fn selected_target(&self) -> Option<(String, String)> {
        let row = self.selected_row()?;
        if let Some(reference) = &row.reference {
            return Some((row.repo.clone(), reference.clone()));
        }
        let repo = self.repos.iter().find(|r| r.id == row.repo)?;
        (!repo.default_branch.is_empty())
            .then(|| (repo.id.clone(), repo.default_branch.clone()))
    }

    /// Identity of the selected row, used to hold the selection across rebuilds.
    fn selected_key(&self) -> Option<(String, Option<String>)> {
        self.selected_row()
            .map(|r| (r.repo.clone(), r.reference.clone()))
    }

    /// Recompute the visible rows, preserving the selection where possible.
    pub(super) fn rebuild(&mut self) {
        let previous = self.selected_key();
        let needle = self.filter.to_lowercase();
        let matches = |s: &str| needle.is_empty() || s.to_lowercase().contains(&needle);

        let mut rows = Vec::new();
        for repo in &self.repos {
            if !matches(&repo.id) {
                continue;
            }
            let expanded = self.expanded.contains(&repo.id);
            let slot = self.refs.get(&repo.id);
            rows.push(ReposRow {
                label: repo.id.clone(),
                meta: repo.default_branch.clone(),
                kind: RowKind::Repo,
                primary: false,
                repo: repo.id.clone(),
                reference: None,
                expanded,
                expandable: match slot {
                    // The full ref list, once fetched, is the authority.
                    Some(s) if matches!(s.load, Load::Ready) => s.has_visible(),
                    // Otherwise the probe, and optimism until it answers.
                    _ => self.probe.get(&repo.id).copied().unwrap_or(true),
                },
                loading: matches!(slot.map(|s| &s.load), Some(Load::Loading)),
            });
            if !expanded {
                continue;
            }
            let Some(slot) = slot else { continue };
            for r in slot.visible() {
                rows.push(ReposRow {
                    label: r.id.clone(),
                    meta: r.commit_id.chars().take(8).collect(),
                    kind: match r.kind {
                        RefKind::Branch => RowKind::Branch,
                        RefKind::Tag => RowKind::Tag,
                    },
                    primary: r.is_default,
                    repo: repo.id.clone(),
                    reference: Some(r.id.clone()),
                    expanded: false,
                    expandable: false,
                    loading: false,
                });
            }
        }
        self.rows = rows;

        let restored = previous.and_then(|(repo, reference)| {
            self.rows
                .iter()
                .position(|r| r.repo == repo && r.reference == reference)
                // The row we were on may have vanished with its parent; fall
                // back to the repository it belonged to.
                .or_else(|| self.rows.iter().position(|r| r.repo == repo))
        });
        self.state.select(match restored {
            Some(i) => Some(i),
            None if self.rows.is_empty() => None,
            None => Some(0),
        });
    }
}

// ── pane 2: the object tree ──────────────────────────────────────────────

/// Arena slot of the tree's synthetic root. Its children are the top level.
pub const ROOT: usize = 0;

pub struct Node {
    pub stat: ObjectStats,
    pub name: String,
    pub depth: usize,
    pub parent: Option<usize>,
    /// Empty until this directory's listing lands.
    pub children: Vec<usize>,
    pub load: Load,
    /// The user's own expand/collapse toggle. A search never touches it, so
    /// clearing the filter restores exactly the shape they had open.
    pub expanded: bool,
    /// Set by the filter pass: this node or one of its descendants matches.
    pub matched: bool,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.stat.is_dir()
    }
}

pub struct TreeView {
    /// (repo, ref) this tree belongs to.
    pub key: Option<(String, String)>,
    /// Arena. `nodes[ROOT]` is synthetic and stands for the ref's top level.
    pub nodes: Vec<Node>,
    /// path -> arena slot, for routing replies back to the right node.
    pub index: HashMap<String, usize>,
    /// Flattened visible slots.
    pub rows: Vec<usize>,
    pub state: ListState,
    pub filter: String,
    /// Set when the filter changed; the network crawl fires once it settles.
    pub filter_dirty: Option<Instant>,
    /// Bumped on repo/ref switch and reload; stale replies are dropped.
    pub generation: u64,
    pub crawling: bool,
    pub capped: bool,
}

impl Default for TreeView {
    fn default() -> Self {
        let mut tree = Self {
            key: None,
            nodes: Vec::new(),
            index: HashMap::new(),
            rows: Vec::new(),
            state: ListState::default(),
            filter: String::new(),
            filter_dirty: None,
            generation: 0,
            crawling: false,
            capped: false,
        };
        tree.clear_arena();
        tree
    }
}

impl TreeView {
    fn clear_arena(&mut self) {
        self.nodes = vec![Node {
            stat: ObjectStats {
                path: String::new(),
                path_type: "common_prefix".into(),
                size_bytes: None,
            },
            name: String::new(),
            depth: 0,
            parent: None,
            children: Vec::new(),
            load: Load::Idle,
            expanded: true,
            matched: false,
        }];
        self.index = HashMap::from([(String::new(), ROOT)]);
        self.rows.clear();
        self.state = ListState::default();
        self.capped = false;
        self.crawling = false;
    }

    /// Point the tree at a new ref, discarding everything loaded for the old
    /// one. Bumps the generation so in-flight replies are ignored.
    pub(super) fn retarget(&mut self, key: Option<(String, String)>) {
        self.generation += 1;
        self.key = key;
        self.clear_arena();
    }

    pub fn root_load(&self) -> &Load {
        &self.nodes[ROOT].load
    }

    pub fn selected_slot(&self) -> Option<usize> {
        self.state.selected().and_then(|i| self.rows.get(i)).copied()
    }

    pub fn selected(&self) -> Option<&Node> {
        self.selected_slot().map(|slot| &self.nodes[slot])
    }

    fn selected_path(&self) -> Option<String> {
        self.selected().map(|n| n.stat.path.clone())
    }

    pub(super) fn select_slot(&mut self, slot: usize) {
        if let Some(row) = self.rows.iter().position(|s| *s == slot) {
            self.state.select(Some(row));
        }
    }

    /// Whether this directory's children are currently shown beneath it.
    pub fn is_open(&self, slot: usize) -> bool {
        let node = &self.nodes[slot];
        if !node.is_dir() || node.children.is_empty() {
            return false;
        }
        if self.filter.is_empty() {
            node.expanded
        } else {
            // Under a filter the shape is driven by matches, and a directory
            // matched only by its own name has nothing shown beneath it.
            node.children.iter().any(|c| self.nodes[*c].matched)
        }
    }

    /// Attach a freshly-listed level to `slot`. Pure arena mutation — callers
    /// re-mark and rebuild once, after applying a whole batch.
    pub(super) fn insert_children(&mut self, slot: usize, entries: Vec<ObjectStats>) {
        let depth = if slot == ROOT {
            0
        } else {
            self.nodes[slot].depth + 1
        };
        let mut children = Vec::with_capacity(entries.len());
        for stat in entries {
            // A level can be listed twice — an expand racing the search crawl.
            // Reuse the existing node so its subtree and toggle survive.
            if let Some(&existing) = self.index.get(&stat.path) {
                children.push(existing);
                continue;
            }
            let name = stat.name().to_string();
            let path = stat.path.clone();
            let is_dir = stat.is_dir();
            self.nodes.push(Node {
                stat,
                name,
                depth,
                parent: Some(slot),
                children: Vec::new(),
                load: if is_dir { Load::Idle } else { Load::Ready },
                expanded: false,
                matched: false,
            });
            let new = self.nodes.len() - 1;
            self.index.insert(path, new);
            children.push(new);
        }
        self.nodes[slot].children = children;
        self.nodes[slot].load = Load::Ready;
    }

    /// Bottom-up match pass. Children always sit at a higher arena index than
    /// their parent, so one reverse sweep resolves the whole tree.
    pub(super) fn mark_matches(&mut self) {
        let needle = self.filter.to_lowercase();
        if needle.is_empty() {
            for node in &mut self.nodes {
                node.matched = false;
            }
            return;
        }
        for slot in (0..self.nodes.len()).rev() {
            let node = &self.nodes[slot];
            let own = !node.name.is_empty() && node.name.to_lowercase().contains(&needle);
            let descendant = node.children.iter().any(|c| self.nodes[*c].matched);
            self.nodes[slot].matched = own || descendant;
        }
    }

    /// Flatten the arena into visible rows, depth-first.
    pub(super) fn rebuild_rows(&mut self) {
        let previous = self.selected_path();
        let filtering = !self.filter.is_empty();

        let mut rows = Vec::new();
        let mut stack: Vec<usize> = self.nodes[ROOT].children.iter().rev().copied().collect();
        while let Some(slot) = stack.pop() {
            let node = &self.nodes[slot];
            if filtering && !node.matched {
                continue;
            }
            rows.push(slot);
            let descend = if filtering { node.matched } else { node.expanded };
            if node.is_dir() && descend {
                stack.extend(node.children.iter().rev().copied());
            }
        }
        self.rows = rows;

        let restored = previous.and_then(|path| {
            self.index
                .get(&path)
                .and_then(|slot| self.rows.iter().position(|s| s == slot))
        });
        self.state.select(match restored {
            Some(i) => Some(i),
            None if self.rows.is_empty() => None,
            None => Some(0),
        });
    }

    /// Directories a search still needs to fetch. Excludes those already
    /// loaded, in flight, or known to have failed, so nothing is fetched twice
    /// and a broken directory isn't retried in a loop.
    pub(super) fn unloaded_dirs(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|n| n.is_dir() && matches!(n.load, Load::Idle))
            .map(|n| n.stat.path.clone())
            .collect()
    }

    /// Objects discovered so far, not counting the synthetic root.
    pub fn discovered(&self) -> usize {
        self.nodes.len() - 1
    }
}

// ── commits tab ──────────────────────────────────────────────────────────

#[derive(Default)]
pub struct CommitsView {
    pub commits: Vec<Commit>,
    pub state: ListState,
    pub load: Option<Load>,
    pub req: u64,
    /// (repo, ref) the loaded commits belong to.
    pub key: Option<(String, String)>,
}

// ── preview ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct Preview {
    pub key: Option<(String, String, String)>,
    pub stat: Option<ObjectStats>,
    pub body: Option<PreviewBody>,
    pub error: Option<String>,
    pub loading: bool,
    pub scroll: u16,
    pub req: u64,
    /// Set when the selection moved; the fetch fires once it settles.
    pub dirty_since: Option<Instant>,
}

pub enum PreviewBody {
    Text(Vec<String>),
    /// Re-indented JSON, tokenised so the UI can colour it, and foldable a level
    /// at a time. Both the side pane and the zoom render this one document, so
    /// what you fold in either is folded in the other. Parsed once here rather
    /// than per frame, and bounded by `preview_bytes`.
    Json(crate::jsonl::JsonDoc),
    /// Newline-delimited JSON, one foldable record per row. The side pane still
    /// renders it as plain text; only the zoom unfolds it.
    Jsonl(crate::jsonl::Doc),
    Binary(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(refs: Vec<NamedRef>) -> RefsSlot {
        RefsSlot {
            load: Load::Ready,
            refs,
            req: 0,
        }
    }

    fn branch(id: &str, is_default: bool) -> NamedRef {
        NamedRef {
            id: id.into(),
            commit_id: "c0ffee".into(),
            kind: RefKind::Branch,
            is_default,
        }
    }

    fn tag(id: &str) -> NamedRef {
        NamedRef {
            id: id.into(),
            commit_id: "c0ffee".into(),
            kind: RefKind::Tag,
            is_default: false,
        }
    }

    fn ids(slot: &RefsSlot) -> Vec<&str> {
        slot.visible().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn a_lone_default_branch_is_not_listed() {
        let slot = slot(vec![branch("main", true)]);
        assert!(ids(&slot).is_empty());
        // The repository row alone reaches it, so there is nothing to expand.
        assert!(!slot.has_visible());
    }

    #[test]
    fn several_branches_are_all_listed() {
        let slot = slot(vec![branch("main", true), branch("dev", false)]);
        assert_eq!(ids(&slot), vec!["main", "dev"]);
        assert!(slot.has_visible());
    }

    #[test]
    fn tags_keep_a_lone_branch_hidden_but_the_repo_expandable() {
        let slot = slot(vec![branch("main", true), tag("v1.0")]);
        assert_eq!(ids(&slot), vec!["v1.0"]);
        assert!(slot.has_visible());
    }

    #[test]
    fn a_lone_branch_that_is_not_the_default_stays() {
        // `selected_target` falls back to the repository's default branch, so a
        // branch it doesn't name would be unreachable if it were hidden.
        let slot = slot(vec![branch("orphan", false)]);
        assert_eq!(ids(&slot), vec!["orphan"]);
    }

    #[test]
    fn refs_still_loading_leave_the_repo_expandable() {
        // Optimistic until the listing lands: the chevron shows up front.
        let row_expandable = |s: &RefsSlot| match s {
            s if matches!(s.load, Load::Ready) => s.has_visible(),
            _ => true,
        };
        let mut pending = slot(vec![]);
        pending.load = Load::Loading;
        assert!(row_expandable(&pending));
        assert!(!row_expandable(&slot(vec![branch("main", true)])));
    }
}
