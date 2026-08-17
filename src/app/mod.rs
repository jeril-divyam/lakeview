//! Application state and the update half of the loop.
//!
//! The browser is three fixed panes: repositories (expandable to reveal their
//! refs), a lazily-loaded tree of one ref's objects, and a detail/preview pane.
//! All network work happens off-thread and reports back through `Msg`; pane-one
//! requests carry a monotonic id and tree requests carry a generation, so stale
//! replies are dropped rather than applied to the wrong thing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use humansize::{DECIMAL, format_size};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{Config, Profile};
use crate::jsonl::Folding;
use crate::keys::{KeyFilter, MenuRow};
use crate::lakefs::{Client, Commit, NamedRef, ObjectStats, RefKind, Repository};
use crate::ui::MIN_REPOS;

mod body;
mod download;
mod mouse;
#[cfg(test)]
pub(crate) mod test_support;

pub use mouse::{Divider, Handle, Hits};
use body::render_body;
use mouse::{Drag, WHEEL_LINES, row_at_line};

const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(150);
/// How long the pane-one selection must settle before its ref's tree is
/// fetched. Without this, holding `j` down the repository list would fire a
/// root listing per row.
const TARGET_DEBOUNCE: Duration = Duration::from_millis(120);
/// How long the filter must sit still before a recursive search goes to the
/// network. Marking already-loaded nodes is not debounced — that is instant.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
const STATUS_TTL: Duration = Duration::from_secs(4);
/// Directory listings a search has in flight at once.
const CRAWL_CONCURRENCY: usize = 8;
/// Repositories whose refs are probed at once on start-up.
const PROBE_CONCURRENCY: usize = 8;
/// Arena slot of the tree's synthetic root. Its children are the top level.
pub const ROOT: usize = 0;

// ── messages from background tasks ───────────────────────────────────────

pub enum Msg {
    Repos(u64, Result<Vec<Repository>, String>),
    Refs {
        req: u64,
        repo: String,
        res: Result<Vec<NamedRef>, String>,
    },
    /// Whether a repository has refs worth listing, answered without fetching
    /// them all. Carries the repository listing's id so a reload's answers
    /// can't land on a newer list.
    RefProbe {
        req: u64,
        repo: String,
        listable: bool,
    },
    /// One directory level, fetched because the user expanded it.
    Children {
        generation: u64,
        prefix: String,
        res: Result<Vec<ObjectStats>, String>,
    },
    /// A level of the recursive search, several listings at a time.
    Crawl {
        generation: u64,
        batch: Vec<(String, Result<Vec<ObjectStats>, String>)>,
        capped: bool,
        done: bool,
    },
    Commits(u64, Result<Vec<Commit>, String>),
    Preview(u64, Result<PreviewPayload, String>),
    /// A finished download: the file written, and its size. Alone among these
    /// it carries no request id — a download is a side effect that happened,
    /// so its outcome is worth reporting however far the selection has moved.
    Download(Result<(String, u64), String>),
}

pub struct PreviewPayload {
    pub stat: ObjectStats,
    pub bytes: Vec<u8>,
    /// The fetch came back at `preview_bytes`, so this is not the whole object.
    pub truncated: bool,
}

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

fn move_in(state: &mut ListState, len: usize, delta: isize) {
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
    fn has_visible(&self) -> bool {
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
    fn rebuild(&mut self) {
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
    fn retarget(&mut self, key: Option<(String, String)>) {
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

    fn select_slot(&mut self, slot: usize) {
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
    fn insert_children(&mut self, slot: usize, entries: Vec<ObjectStats>) {
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
    fn mark_matches(&mut self) {
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
    fn rebuild_rows(&mut self) {
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
    fn unloaded_dirs(&self) -> Vec<String> {
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

// ── modes & tabs ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Browse,
    Commits,
    Help,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Browse, Tab::Commits, Tab::Help];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Browse => "Browse",
            Tab::Commits => "Commits",
            Tab::Help => "Help",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Typing into the focused pane's filter.
    Filter,
    /// Full-screen preview of the selected object.
    Zoom,
    /// Profile picker overlay.
    Profiles(usize),
    /// The key-filter menu, floating over a zoomed JSONL preview. The zoom is
    /// still what is drawn behind it — see [`App::zoomed`].
    Keys,
}

pub struct Status {
    pub text: String,
    pub is_error: bool,
    pub at: Instant,
}

// ── the key-filter menu ──────────────────────────────────────────────────

/// Everything the key menu needs that isn't the filter itself. The filter lives
/// on the document it filters, so it lasts exactly as long as the preview does;
/// this is the overlay's own cursor, and the copy `Esc` puts back.
#[derive(Default)]
pub struct KeysMenu {
    pub cursor: usize,
    pub scroll: usize,
    /// Height of the menu's list area, refreshed on each draw.
    pub viewport: usize,
    /// The filter as it was when the menu opened. Edits show through the menu
    /// as they are made — the file is right there behind it — so cancelling
    /// means putting this back rather than never having applied anything.
    undo: Option<KeyFilter>,
    /// Whether anything was actually switched, so closing an untouched menu
    /// leaves the document alone.
    edited: bool,
    /// Rects of the last draw: the whole popup, and the list inside it.
    pub popup: Rect,
    pub list: Rect,
}

// ── app ──────────────────────────────────────────────────────────────────

pub struct App {
    pub cfg: Config,
    pub profile_name: String,
    pub profile: Profile,
    pub client: Client,
    pub tx: UnboundedSender<Msg>,

    pub repos: ReposView,
    pub tree: TreeView,
    pub focus: Focus,
    pub tab: Tab,
    pub mode: Mode,
    pub commits: CommitsView,
    pub preview: Preview,
    pub keys: KeysMenu,
    pub status: Option<Status>,
    pub should_quit: bool,

    next_req: u64,
    pub inflight: usize,
    pub tick: usize,
    /// Shared with the search crawler so a new search, a cleared filter or a
    /// change of ref actually stops the old one rather than just ignoring it.
    crawl_token: Arc<AtomicU64>,
    /// A `--repo`/`--ref` jump waiting for the repository list to arrive.
    pending_jump: Option<(String, Option<String>)>,
    /// Repository whose refs `→` is waiting on. If they turn out to hold
    /// nothing worth listing, the move carries on into the tree instead of
    /// leaving the keypress looking like it did nothing.
    descend_after_refs: Option<String>,
    /// Directories a reload should re-open, applied as each level lands.
    pending_expand: Vec<String>,
    /// Set when the pane-one selection moved; the tree follows once it settles.
    target_dirty: Option<Instant>,
    /// Regions recorded by the last render, for mouse hit-testing.
    pub hits: Hits,
    /// Cell and time of the last click, used to detect double-clicks.
    last_click: Option<(u16, u16, Instant)>,
    /// The pane border the mouse has hold of, if any.
    drag: Option<Drag>,
    /// Width the repositories pane had before it was folded, so unfolding lands
    /// where it was rather than at a default. Held for the session only: the file
    /// records that the pane is folded, which is the part worth keeping.
    repos_restore: u16,
    /// Fold shapes of the `.json` objects visited this session, keyed the way the
    /// preview is, oldest first. A preview lasts only as long as the selection
    /// sits on it, so without this every fold would be undone by a glance at the
    /// file next door. Trimmed from the front past [`FOLD_MEMORY`]: a long browse
    /// must not grow without bound, and an object folded thirty files back is not
    /// worth the room.
    folds: Vec<((String, String, String), crate::jsonl::Open)>,
}

/// How many objects' fold shapes are kept. Comfortably more than anyone flips
/// between while reading, and small enough that the whole lot is a linear scan.
const FOLD_MEMORY: usize = 64;

impl App {
    pub fn new(
        cfg: Config,
        profile_name: String,
        profile: Profile,
        client: Client,
        tx: UnboundedSender<Msg>,
    ) -> Self {
        // A config that starts folded has no width to go back to, so unfolding
        // lands on the floor.
        let repos_restore = cfg.ui.repos_width.max(MIN_REPOS);
        let mut app = Self {
            cfg,
            profile_name,
            profile,
            client,
            tx,
            repos: ReposView::default(),
            tree: TreeView::default(),
            focus: Focus::Repos,
            tab: Tab::Browse,
            mode: Mode::Normal,
            commits: CommitsView::default(),
            preview: Preview::default(),
            keys: KeysMenu::default(),
            status: None,
            should_quit: false,
            next_req: 0,
            inflight: 0,
            tick: 0,
            crawl_token: Arc::new(AtomicU64::new(0)),
            pending_jump: None,
            descend_after_refs: None,
            pending_expand: Vec::new(),
            target_dirty: None,
            hits: Hits::default(),
            last_click: None,
            drag: None,
            repos_restore,
            folds: Vec::new(),
        };
        app.load_repos();
        if let Some(warning) = app.cfg.warnings.first().cloned() {
            app.set_status(warning, true);
        }
        app
    }

    fn req_id(&mut self) -> u64 {
        self.next_req += 1;
        self.next_req
    }

    pub fn set_status(&mut self, text: impl Into<String>, is_error: bool) {
        self.status = Some(Status {
            text: text.into(),
            is_error,
            at: Instant::now(),
        });
    }

    /// Anything on the wire, for the header spinner.
    pub fn busy(&self) -> bool {
        self.inflight > 0 || self.tree.crawling
    }

    /// The repo/ref the tree is showing, if any.
    pub fn context(&self) -> Option<(&str, &str)> {
        self.tree
            .key
            .as_ref()
            .map(|(repo, reference)| (repo.as_str(), reference.as_str()))
    }

    /// Whether the preview has the screen to itself. The key menu is a panel
    /// over the zoom rather than a place of its own, so the zoom is still what
    /// is drawn, moved and folded underneath it.
    pub fn zoomed(&self) -> bool {
        matches!(self.mode, Mode::Zoom | Mode::Keys)
    }

    /// The zoomed JSONL document, when that is what is on screen. The
    /// `truncated` mark and the key filter are particular to it; folding goes
    /// through `focused_doc`.
    pub fn jsonl(&self) -> Option<&crate::jsonl::Doc> {
        match (self.zoomed(), &self.preview.body) {
            (true, Some(PreviewBody::Jsonl(doc))) => Some(doc),
            _ => None,
        }
    }

    fn jsonl_mut(&mut self) -> Option<&mut crate::jsonl::Doc> {
        match (self.zoomed(), &mut self.preview.body) {
            (true, Some(PreviewBody::Jsonl(doc))) => Some(doc),
            _ => None,
        }
    }

    /// Whether the keys are driving a foldable document rather than a list of
    /// rows: the zoom, or the preview pane holding a `.json`.
    ///
    /// The focus outlives a change of tab — come back to Browse and the cursor
    /// is where you left it — so the tab is part of the question. Otherwise `a`
    /// or `G` on the commit log would fold a file nobody can see.
    pub fn driving_doc(&self) -> bool {
        self.tab == Tab::Browse && (self.zoomed() || self.focus == Focus::Preview)
    }

    /// The foldable document the keys are driving, of either kind.
    ///
    /// Zoomed, that is whatever is on screen. In the three-pane layout it is the
    /// preview once `→` has stepped into it, and then only a `.json`: a `.jsonl`
    /// record is too much to read at that width, so beside the tree it stays the
    /// flat line it is and folds in the zoom alone.
    pub fn focused_doc(&self) -> Option<&dyn Folding> {
        if !self.driving_doc() {
            return None;
        }
        match (&self.preview.body, self.zoomed()) {
            (Some(PreviewBody::Jsonl(doc)), true) => Some(doc),
            (Some(PreviewBody::Json(doc)), _) => Some(doc),
            _ => None,
        }
    }

    fn focused_doc_mut(&mut self) -> Option<&mut dyn Folding> {
        if !self.driving_doc() {
            return None;
        }
        let zoomed = self.zoomed();
        match (&mut self.preview.body, zoomed) {
            (Some(PreviewBody::Jsonl(doc)), true) => Some(doc),
            (Some(PreviewBody::Json(doc)), _) => Some(doc),
            _ => None,
        }
    }

    /// The previewed JSON document whatever the focus, which is what the pane
    /// draws and what the fold memory is taken from.
    fn json_doc_mut(&mut self) -> Option<&mut crate::jsonl::JsonDoc> {
        match &mut self.preview.body {
            Some(PreviewBody::Json(doc)) => Some(doc),
            _ => None,
        }
    }

    /// Whether the preview is holding a `.json` the cursor could step into.
    pub fn preview_folds(&self) -> bool {
        !self.preview.loading
            && self.preview.error.is_none()
            && matches!(self.preview.body, Some(PreviewBody::Json(_)))
    }

    /// The filter belonging to whichever pane has focus. The preview searches
    /// nothing — it is one file, not a list — so it has none.
    pub fn filter(&self) -> &str {
        match self.focus {
            Focus::Repos => &self.repos.filter,
            Focus::Tree => &self.tree.filter,
            Focus::Preview => "",
        }
    }

    // ── loading ──────────────────────────────────────────────────────────

    pub fn load_repos(&mut self) {
        let req = self.req_id();
        self.repos.req = req;
        self.inflight += 1;
        let (tx, client) = (self.tx.clone(), self.client.clone());
        tokio::spawn(async move {
            let res = client.repositories().await.map_err(fmt_err);
            let _ = tx.send(Msg::Repos(req, res));
        });
    }

    /// Ask every repository whether it has refs worth listing, so the pane's
    /// chevrons are right from the start rather than correcting themselves as
    /// you open things. One or two capped listings per repository, eight at a
    /// time; repositories whose refs are already in hand are skipped.
    fn probe_refs(&mut self) {
        let todo: Vec<(String, String)> = self
            .repos
            .repos
            .iter()
            .filter(|r| {
                !self.repos.probe.contains_key(&r.id)
                    && !matches!(
                        self.repos.refs.get(&r.id).map(|s| &s.load),
                        Some(Load::Ready) | Some(Load::Loading)
                    )
            })
            .map(|r| (r.id.clone(), r.default_branch.clone()))
            .collect();
        if todo.is_empty() {
            return;
        }

        let req = self.repos.req;
        let include_tags = self.cfg.ui.show_tags;
        self.inflight += todo.len();
        let (tx, client) = (self.tx.clone(), self.client.clone());
        tokio::spawn(async move {
            futures::stream::iter(todo)
                .for_each_concurrent(PROBE_CONCURRENCY, |(repo, default_branch)| {
                    let (tx, client) = (tx.clone(), client.clone());
                    async move {
                        // A probe that fails leaves the repository looking
                        // expandable; opening it will report the error itself.
                        let listable = client
                            .has_listable_refs(&repo, &default_branch, include_tags)
                            .await
                            .unwrap_or(true);
                        let _ = tx.send(Msg::RefProbe {
                            req,
                            repo,
                            listable,
                        });
                    }
                })
                .await;
        });
    }

    fn load_refs(&mut self, repo: &str) {
        let req = self.req_id();
        self.repos.refs.insert(
            repo.to_string(),
            RefsSlot {
                load: Load::Loading,
                refs: Vec::new(),
                req,
            },
        );
        self.inflight += 1;
        let show_tags = self.cfg.ui.show_tags;
        // The listing we expanded from already knows the default branch.
        let default_branch = self
            .repos
            .repos
            .iter()
            .find(|r| r.id == repo)
            .map(|r| r.default_branch.clone())
            .unwrap_or_default();
        let (tx, client, repo) = (self.tx.clone(), self.client.clone(), repo.to_string());
        tokio::spawn(async move {
            let res = client
                .refs(&repo, &default_branch, show_tags)
                .await
                .map_err(fmt_err);
            let _ = tx.send(Msg::Refs { req, repo, res });
        });
    }

    /// Fetch one directory level into `slot`.
    fn load_children(&mut self, slot: usize) {
        let Some((repo, reference)) = self.tree.key.clone() else {
            return;
        };
        let prefix = self.tree.nodes[slot].stat.path.clone();
        self.tree.nodes[slot].load = Load::Loading;
        let generation = self.tree.generation;
        self.inflight += 1;
        let (tx, client) = (self.tx.clone(), self.client.clone());
        tokio::spawn(async move {
            let res = client
                .list_objects(&repo, &reference, &prefix)
                .await
                .map_err(fmt_err);
            let _ = tx.send(Msg::Children {
                generation,
                prefix,
                res,
            });
        });
    }

    /// Point the tree at `(repo, reference)` and start loading its top level.
    fn set_target(&mut self, target: Option<(String, String)>) {
        if self.tree.key == target {
            return;
        }
        self.tree.retarget(target);
        self.bump_crawl();
        self.clear_preview();
        if self.tree.key.is_some() {
            self.load_children(ROOT);
            // Carry an active search over to the new ref.
            if !self.tree.filter.is_empty() {
                self.tree.filter_dirty = Some(Instant::now());
            }
        }
    }

    /// Note that the tree should follow the pane-one selection. The move is
    /// debounced, so running down the repository list costs one fetch, not one
    /// per row.
    fn sync_target(&mut self) {
        self.target_dirty = Some(Instant::now());
    }

    /// Called on every tick: moves the tree to the selected repo/ref.
    fn poll_target(&mut self) {
        let Some(since) = self.target_dirty else {
            return;
        };
        if since.elapsed() < TARGET_DEBOUNCE {
            return;
        }
        self.target_dirty = None;

        let target = self.repos.selected_target();
        if self.tree.key == target {
            return;
        }
        // A repository row stands for its default branch, but only when you
        // are not already inside that repository. Otherwise collapsing it — or
        // just moving up onto its row — would yank the tree off the ref you
        // were reading. Picking a ref row explicitly always moves the tree.
        let on_repo_row = self
            .repos
            .selected_row()
            .is_some_and(|r| r.reference.is_none());
        if on_repo_row
            && let (Some((selected, _)), Some((showing, _))) = (&target, &self.tree.key)
            && selected == showing
        {
            return;
        }

        self.set_target(target);
        // `set_target` resets the preview, so re-arm it.
        self.mark_preview_dirty();
    }

    pub fn reload_focused(&mut self) {
        match self.focus {
            Focus::Repos => {
                self.repos.refs.clear();
                self.repos.probe.clear();
                self.load_repos();
                self.set_status("reloading repositories…", false);
            }
            // In the preview `r` means the object under it, not the tree around
            // it. Dropping the key is what makes the next poll refetch; the
            // folds are put away first so the reload comes back as you left it.
            Focus::Preview => {
                self.remember_folds();
                self.preview.key = None;
                self.mark_preview_dirty();
                self.set_status("reloading preview…", false);
            }
            Focus::Tree => {
                let Some(key) = self.tree.key.clone() else {
                    return;
                };
                // Keep the shape the user had open across the reload.
                let open: Vec<String> = self
                    .tree
                    .nodes
                    .iter()
                    .filter(|n| n.expanded && n.is_dir() && n.parent.is_some())
                    .map(|n| n.stat.path.clone())
                    .collect();
                self.tree.retarget(Some(key));
                self.pending_expand = open;
                self.bump_crawl();
                self.load_children(ROOT);
                self.clear_preview();
                if !self.tree.filter.is_empty() {
                    self.tree.filter_dirty = Some(Instant::now());
                }
                self.set_status("reloading…", false);
            }
        }
    }

    pub fn load_commits(&mut self, force: bool) {
        let Some((repo, reference)) = self.context().map(|(r, f)| (r.to_string(), f.to_string()))
        else {
            return;
        };
        if !force && self.commits.key.as_ref() == Some(&(repo.clone(), reference.clone())) {
            return;
        }

        let req = self.req_id();
        self.commits.req = req;
        self.commits.load = Some(Load::Loading);
        self.commits.key = Some((repo.clone(), reference.clone()));
        self.commits.commits.clear();
        self.commits.state.select(None);
        self.inflight += 1;

        let (tx, client) = (self.tx.clone(), self.client.clone());
        tokio::spawn(async move {
            let res = client.commits(&repo, &reference).await.map_err(fmt_err);
            let _ = tx.send(Msg::Commits(req, res));
        });
    }

    pub fn mark_preview_dirty(&mut self) {
        self.preview.dirty_since = Some(Instant::now());
    }

    /// Called on every tick: fires the debounced preview fetch.
    pub fn poll_preview(&mut self) {
        let Some(since) = self.preview.dirty_since else {
            return;
        };
        if since.elapsed() < PREVIEW_DEBOUNCE {
            return;
        }
        self.preview.dirty_since = None;

        let Some((repo, reference)) = self.context().map(|(r, f)| (r.to_string(), f.to_string()))
        else {
            self.clear_preview();
            return;
        };
        // Only the tree selects objects; pane one renders its own details. The
        // preview is reading whatever the tree is on, so it holds the selection
        // still rather than dropping it.
        let object = match self.focus {
            Focus::Repos => None,
            Focus::Tree | Focus::Preview => self.tree.selected().map(|n| n.stat.clone()),
        };
        let Some(object) = object else {
            self.clear_preview();
            return;
        };
        if object.is_dir() {
            self.clear_preview();
            self.preview.stat = Some(object);
            return;
        }

        let key = (repo.clone(), reference.clone(), object.path.clone());
        if self.preview.key.as_ref() == Some(&key) {
            return;
        }
        // Past here the old preview is replaced either way, so its folds are put
        // away while its own key is still the one on record.
        self.remember_folds();

        // A ranged GET on a zero-byte object can come back 416; skip the trip.
        if object.size_bytes == Some(0) {
            self.preview = Preview {
                key: Some(key),
                stat: Some(object),
                body: Some(PreviewBody::Text(Vec::new())),
                ..Default::default()
            };
            return;
        }

        let req = self.req_id();
        self.preview = Preview {
            key: Some(key),
            stat: Some(object.clone()),
            loading: true,
            req,
            ..Default::default()
        };
        self.inflight += 1;

        let (tx, client) = (self.tx.clone(), self.client.clone());
        let limit = self.cfg.ui.preview_bytes;
        tokio::spawn(async move {
            let res = client
                .get_object_head(&repo, &reference, &object.path, limit)
                .await
                .map(|(bytes, truncated)| PreviewPayload {
                    truncated,
                    stat: object,
                    bytes,
                })
                .map_err(fmt_err);
            let _ = tx.send(Msg::Preview(req, res));
        });
    }

    fn clear_preview(&mut self) {
        self.remember_folds();
        self.preview = Preview::default();
        // Nothing left in there to read, so the cursor cannot stay in it.
        self.leave_preview();
    }

    /// Put the previewed `.json`'s fold shape away before the preview holding it
    /// goes. Anything else — a `.jsonl`'s records, its key filter, plain text —
    /// has no shape worth keeping and is left to go with it.
    fn remember_folds(&mut self) {
        let (Some(key), Some(PreviewBody::Json(doc))) = (&self.preview.key, &self.preview.body)
        else {
            return;
        };
        let folds = doc.folds();
        match self.folds.iter().position(|(k, _)| k == key) {
            // Re-recorded at the back: what you were just reading is the last
            // thing worth dropping.
            Some(i) => {
                let (key, _) = self.folds.remove(i);
                self.folds.push((key, folds));
            }
            None => self.folds.push((key.clone(), folds)),
        }
        let over = self.folds.len().saturating_sub(FOLD_MEMORY);
        self.folds.drain(..over);
    }

    /// The fold shape kept for the object the preview is now on, if it has been
    /// read before. Copied rather than taken, so a body that turns out not to be
    /// JSON after all doesn't quietly throw the shape away; `remember_folds`
    /// overwrites the entry when the preview goes.
    fn recall_folds(&self) -> Option<crate::jsonl::Open> {
        let key = self.preview.key.as_ref()?;
        let (_, folds) = self.folds.iter().find(|(k, _)| k == key)?;
        Some(folds.clone())
    }

    // ── recursive search ─────────────────────────────────────────────────

    /// Invalidate any crawl in flight. The task checks this between levels.
    fn bump_crawl(&mut self) -> u64 {
        self.tree.crawling = false;
        self.crawl_token.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Called on every tick: starts the debounced network crawl.
    fn poll_search(&mut self) {
        let Some(since) = self.tree.filter_dirty else {
            return;
        };
        if since.elapsed() < SEARCH_DEBOUNCE {
            return;
        }
        self.tree.filter_dirty = None;
        if self.tree.filter.is_empty() || self.tree.key.is_none() {
            return;
        }
        self.start_crawl();
    }

    /// Walk every directory the tree hasn't loaded yet, so a filter can match
    /// things inside collapsed directories. Loaded levels are never re-fetched,
    /// so extending the filter string costs nothing.
    fn start_crawl(&mut self) {
        let Some((repo, reference)) = self.tree.key.clone() else {
            return;
        };
        let token = self.bump_crawl();
        let seed = self.tree.unloaded_dirs();
        if seed.is_empty() {
            return;
        }

        self.tree.crawling = true;
        self.tree.capped = false;
        let generation = self.tree.generation;
        let budget = self.cfg.ui.search_max_requests.max(1);
        let shared = self.crawl_token.clone();
        let (tx, client) = (self.tx.clone(), self.client.clone());

        tokio::spawn(async move {
            let mut frontier = seed;
            let mut remaining = budget;
            loop {
                // A newer search, a cleared filter or a change of ref wins.
                if shared.load(Ordering::Relaxed) != token {
                    return;
                }
                if frontier.is_empty() || remaining == 0 {
                    let _ = tx.send(Msg::Crawl {
                        generation,
                        batch: Vec::new(),
                        capped: !frontier.is_empty(),
                        done: true,
                    });
                    return;
                }

                let take = frontier.len().min(remaining);
                let level: Vec<String> = frontier.drain(..take).collect();
                remaining -= take;

                let batch: Vec<(String, Result<Vec<ObjectStats>, String>)> =
                    futures::stream::iter(level)
                        .map(|prefix| {
                            let client = client.clone();
                            let (repo, reference) = (repo.clone(), reference.clone());
                            async move {
                                let res = client
                                    .list_objects(&repo, &reference, &prefix)
                                    .await
                                    .map_err(fmt_err);
                                (prefix, res)
                            }
                        })
                        .buffer_unordered(CRAWL_CONCURRENCY)
                        .collect()
                        .await;

                // Every directory found here is new — its parent was unloaded,
                // so none of these nodes existed before.
                for (_, res) in &batch {
                    if let Ok(entries) = res {
                        frontier.extend(
                            entries
                                .iter()
                                .filter(|e| e.is_dir())
                                .map(|e| e.path.clone()),
                        );
                    }
                }

                if tx
                    .send(Msg::Crawl {
                        generation,
                        batch,
                        capped: false,
                        done: false,
                    })
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    // ── message handling ─────────────────────────────────────────────────

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Repos(req, res) => {
                self.inflight = self.inflight.saturating_sub(1);
                if self.repos.req != req {
                    return;
                }
                match res {
                    Ok(repos) => {
                        self.repos.repos = repos;
                        // Drop expansions for repositories that are gone, and
                        // re-fetch the refs of those a reload left open.
                        let live: HashSet<String> =
                            self.repos.repos.iter().map(|r| r.id.clone()).collect();
                        self.repos.expanded.retain(|id| live.contains(id));
                        self.repos.probe.retain(|id, _| live.contains(id));
                        let stale: Vec<String> = self
                            .repos
                            .expanded
                            .iter()
                            .filter(|id| !self.repos.refs.contains_key(*id))
                            .cloned()
                            .collect();
                        for repo in stale {
                            self.load_refs(&repo);
                        }
                        self.repos.rebuild();
                        self.apply_jump();
                        // After the jump, so a repository it already opened
                        // isn't probed for what its refs will answer anyway.
                        self.probe_refs();
                        self.sync_target();
                        self.mark_preview_dirty();
                    }
                    Err(e) => {
                        self.repos.repos.clear();
                        self.repos.rebuild();
                        self.set_status(e, true);
                    }
                }
            }

            Msg::Refs { req, repo, res } => {
                self.inflight = self.inflight.saturating_sub(1);
                if self.repos.refs.get(&repo).map(|s| s.req) != Some(req) {
                    return;
                }
                let descending = self.descend_after_refs.as_deref() == Some(repo.as_str());
                if descending {
                    self.descend_after_refs = None;
                }
                match res {
                    Ok(refs) => {
                        let mut empty = false;
                        if let Some(slot) = self.repos.refs.get_mut(&repo) {
                            slot.refs = refs;
                            slot.load = Load::Ready;
                            empty = !slot.has_visible();
                        }
                        // A repository whose only branch is its default has
                        // nothing to list; don't leave it looking open with
                        // nothing under it.
                        if empty {
                            self.repos.expanded.remove(&repo);
                        }
                        self.repos.rebuild();
                        // `→` on such a repository means "descend", so finish
                        // the move now that we know no ref list is in the way.
                        if empty
                            && descending
                            && self.focus == Focus::Repos
                            && self
                                .repos
                                .selected_row()
                                .is_some_and(|r| r.repo == repo && r.reference.is_none())
                        {
                            self.focus = Focus::Tree;
                            self.mark_preview_dirty();
                        }
                        self.apply_jump_ref(&repo);
                    }
                    Err(e) => {
                        if let Some(slot) = self.repos.refs.get_mut(&repo) {
                            slot.load = Load::Failed(e.clone());
                        }
                        self.repos.expanded.remove(&repo);
                        self.repos.rebuild();
                        self.set_status(e, true);
                    }
                }
            }

            Msg::RefProbe {
                req,
                repo,
                listable,
            } => {
                self.inflight = self.inflight.saturating_sub(1);
                if self.repos.req != req {
                    return;
                }
                self.repos.probe.insert(repo, listable);
                self.repos.rebuild();
            }

            Msg::Children {
                generation,
                prefix,
                res,
            } => {
                self.inflight = self.inflight.saturating_sub(1);
                if generation != self.tree.generation {
                    return;
                }
                let Some(&slot) = self.tree.index.get(&prefix) else {
                    return;
                };
                match res {
                    Ok(entries) => {
                        self.tree.insert_children(slot, entries);
                        self.apply_pending_expand();
                        self.tree.mark_matches();
                        self.tree.rebuild_rows();
                        if slot == ROOT && self.focus == Focus::Tree {
                            self.mark_preview_dirty();
                        }
                        // Newly revealed directories may hide search matches.
                        if !self.tree.filter.is_empty() {
                            self.tree.filter_dirty = Some(Instant::now());
                        }
                    }
                    Err(e) => {
                        self.tree.nodes[slot].load = Load::Failed(e.clone());
                        self.tree.rebuild_rows();
                        self.set_status(e, true);
                    }
                }
            }

            Msg::Crawl {
                generation,
                batch,
                capped,
                done,
            } => self.apply_crawl(generation, batch, capped, done),

            Msg::Commits(req, res) => {
                self.inflight = self.inflight.saturating_sub(1);
                if self.commits.req != req {
                    return;
                }
                match res {
                    Ok(commits) => {
                        self.commits
                            .state
                            .select((!commits.is_empty()).then_some(0));
                        self.commits.commits = commits;
                        self.commits.load = Some(Load::Ready);
                    }
                    Err(e) => {
                        self.commits.load = Some(Load::Failed(e.clone()));
                        self.commits.key = None;
                        self.set_status(e, true);
                    }
                }
            }

            Msg::Preview(req, res) => {
                self.inflight = self.inflight.saturating_sub(1);
                if self.preview.req != req {
                    return;
                }
                self.preview.loading = false;
                match res {
                    Ok(payload) => {
                        self.preview.body = Some(render_body(
                            &payload.stat.path,
                            &payload.bytes,
                            payload.truncated,
                        ));
                        // Back to the shape it was left in, if this object has
                        // been read before. A `.json` opens expanded otherwise.
                        if let Some(folds) = self.recall_folds()
                            && let Some(doc) = self.json_doc_mut()
                        {
                            doc.restore(folds);
                        }
                        self.preview.stat = Some(payload.stat);
                    }
                    Err(e) => self.preview.error = Some(e),
                }
                // A reload can land something with nothing to fold — the object
                // rewritten as plain text, or an error where the body was. The
                // cursor has no business staying in a pane it cannot drive.
                if !self.preview_folds() {
                    self.leave_preview();
                }
            }

            Msg::Download(res) => {
                self.inflight = self.inflight.saturating_sub(1);
                match res {
                    Ok((name, bytes)) => self.set_status(
                        format!("downloaded {name} ({})", format_size(bytes, DECIMAL)),
                        false,
                    ),
                    Err(e) => self.set_status(e, true),
                }
            }
        }
    }

    fn apply_crawl(
        &mut self,
        generation: u64,
        batch: Vec<(String, Result<Vec<ObjectStats>, String>)>,
        capped: bool,
        done: bool,
    ) {
        if generation != self.tree.generation {
            return;
        }
        for (prefix, res) in batch {
            let Some(&slot) = self.tree.index.get(&prefix) else {
                continue;
            };
            match res {
                Ok(entries) => self.tree.insert_children(slot, entries),
                Err(e) => self.tree.nodes[slot].load = Load::Failed(e),
            }
        }
        self.tree.mark_matches();
        self.tree.rebuild_rows();

        // Say nothing if the user has moved on from the search.
        if self.tree.filter.is_empty() {
            self.tree.crawling = !done;
            return;
        }
        let found = self.tree.discovered();
        if done {
            self.tree.crawling = false;
            self.tree.capped = capped;
            if capped {
                self.set_status(
                    format!(
                        "search stopped after {} listings — narrow the filter or open a subdirectory",
                        self.cfg.ui.search_max_requests.max(1)
                    ),
                    true,
                );
            } else {
                self.set_status(format!("searched {found} objects"), false);
            }
        } else {
            self.set_status(format!("searching… {found} objects"), false);
        }
    }

    /// Select the repository named by `--repo`, expanding it when a `--ref` was
    /// given too so the ref row can be selected once its listing lands.
    fn apply_jump(&mut self) {
        let Some((repo, reference)) = self.pending_jump.clone() else {
            return;
        };
        let Some(row) = self.repos.rows.iter().position(|r| r.repo == repo) else {
            self.pending_jump = None;
            self.set_status(format!("repository `{repo}` not found"), true);
            return;
        };
        self.repos.state.select(Some(row));
        match reference {
            Some(_) => self.expand_repo(&repo),
            None => self.pending_jump = None,
        }
    }

    fn apply_jump_ref(&mut self, repo: &str) {
        let Some((wanted_repo, Some(reference))) = self.pending_jump.clone() else {
            return;
        };
        if wanted_repo != repo {
            return;
        }
        self.pending_jump = None;
        if let Some(row) = self
            .repos
            .rows
            .iter()
            .position(|r| r.repo == repo && r.reference.as_deref() == Some(reference.as_str()))
        {
            self.repos.state.select(Some(row));
            self.sync_target();
            return;
        }
        // A lone default branch has no row of its own — the repository row
        // `apply_jump` already selected stands for it, and the tree is loading
        // it. Only a ref that genuinely isn't there is worth reporting.
        let exists = self
            .repos
            .refs
            .get(repo)
            .is_some_and(|slot| slot.refs.iter().any(|r| r.id == reference));
        if !exists {
            self.set_status(format!("ref `{reference}` not found in `{repo}`"), true);
        }
    }

    /// Re-open the directories a reload was asked to preserve. A path deeper
    /// than the level that just landed isn't in the arena yet, so it stays on
    /// the list and is retried when its own parent arrives.
    fn apply_pending_expand(&mut self) {
        if self.pending_expand.is_empty() {
            return;
        }
        let mut waiting = Vec::new();
        for path in std::mem::take(&mut self.pending_expand) {
            let Some(&slot) = self.tree.index.get(&path) else {
                waiting.push(path);
                continue;
            };
            self.tree.nodes[slot].expanded = true;
            if matches!(self.tree.nodes[slot].load, Load::Idle) {
                self.load_children(slot);
            }
        }
        self.pending_expand = waiting;
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        // Before the preview, so it sees the ref the tree has settled on.
        self.poll_target();
        self.poll_preview();
        self.poll_search();
        if let Some(status) = &self.status
            && status.at.elapsed() > STATUS_TTL
        {
            self.status = None;
        }
    }

    // ── navigation ───────────────────────────────────────────────────────

    pub fn move_selection(&mut self, delta: isize) {
        match self.tab {
            Tab::Commits => {
                let len = self.commits.commits.len();
                move_in(&mut self.commits.state, len, delta);
            }
            // A foldable document has rows to move between; anything else zoomed
            // is a flat body, and moves the view itself.
            _ if self.driving_doc() => match self.focused_doc_mut() {
                Some(doc) => doc.move_cursor(delta),
                None => {
                    self.preview.scroll = self
                        .preview
                        .scroll
                        .saturating_add_signed(delta.clamp(-1000, 1000) as i16);
                }
            },
            _ => match self.focus {
                Focus::Repos => {
                    let len = self.repos.rows.len();
                    move_in(&mut self.repos.state, len, delta);
                    self.sync_target();
                    self.mark_preview_dirty();
                }
                Focus::Tree => {
                    let len = self.tree.rows.len();
                    move_in(&mut self.tree.state, len, delta);
                    self.mark_preview_dirty();
                }
                // Taken by the arm above, which is the only way focus is here.
                Focus::Preview => {}
            },
        }
    }

    /// `Ctrl-f` / `Ctrl-b` — a screenful at a time through the foldable
    /// document the keys are driving: the zoom, or the preview once the cursor
    /// has stepped into a `.json`. The list panes have `Ctrl-d` and `Ctrl-u`,
    /// and a flat body has no rows to page between.
    ///
    /// Where `Ctrl-d` moves the selection and leaves the view to chase it, this
    /// moves the view and brings the selection along to the top of it. The row
    /// straddling the edge is carried over whole rather than split between the
    /// two pages, so a page always starts where a row does — which is the same
    /// row of overlap you would get anywhere else.
    pub fn page(&mut self, down: bool) {
        let Some(last_row) = self
            .focused_doc()
            .map(|doc| doc.rows_len().saturating_sub(1))
        else {
            return;
        };
        // Without a frame to measure there is no page to speak of yet.
        let starts = &self.hits.preview_row_starts;
        let height = self.hits.preview.map_or(0, |a| a.height as usize);
        if starts.is_empty() || height == 0 {
            return;
        }

        let top = self.preview.scroll as usize;
        let current = row_at_line(starts, top);
        let edge = if down {
            top + height - 1
        } else {
            top.saturating_sub(height - 1)
        };
        let mut row = row_at_line(starts, edge);
        // A row taller than the pane straddles both edges at once, and the view
        // is held to whole rows, so paging to it would land back where it
        // started. Step over it instead of paging onto itself for ever.
        if row == current {
            row = if down {
                current + 1
            } else {
                current.saturating_sub(1)
            };
        }
        let row = row.min(last_row).min(starts.len() - 1);

        self.preview.scroll = starts[row].min(u16::MAX as usize) as u16;
        if let Some(doc) = self.focused_doc_mut() {
            doc.set_cursor(row);
        }
    }

    pub fn select_edge(&mut self, first: bool) {
        if self.driving_doc() {
            match self.focused_doc_mut() {
                Some(doc) => doc.select_edge(first),
                // The render clamps this to the body's real height, which is
                // the only place the wrapped line count is known.
                None => self.preview.scroll = if first { 0 } else { u16::MAX },
            }
            return;
        }

        let (state, len) = match self.tab {
            Tab::Commits => (&mut self.commits.state, self.commits.commits.len()),
            _ => match self.focus {
                Focus::Repos => (&mut self.repos.state, self.repos.rows.len()),
                // The preview is taken by the arm above.
                Focus::Tree | Focus::Preview => (&mut self.tree.state, self.tree.rows.len()),
            },
        };
        if len > 0 {
            state.select(Some(if first { 0 } else { len - 1 }));
        }
        if self.tab != Tab::Commits {
            if self.focus == Focus::Repos {
                self.sync_target();
            }
            self.mark_preview_dirty();
        }
    }

    fn expand_repo(&mut self, repo: &str) {
        self.repos.expanded.insert(repo.to_string());
        let needs_fetch = !matches!(
            self.repos.refs.get(repo).map(|s| &s.load),
            Some(Load::Ready) | Some(Load::Loading)
        );
        if needs_fetch {
            self.load_refs(repo);
        }
        self.repos.rebuild();
    }

    fn collapse_repo(&mut self, repo: &str) {
        self.repos.expanded.remove(repo);
        self.repos.rebuild();
        // Land on the repository row the refs belonged to.
        if let Some(row) = self
            .repos
            .rows
            .iter()
            .position(|r| r.repo == repo && r.reference.is_none())
        {
            self.repos.state.select(Some(row));
            self.sync_target();
        }
    }

    /// `→` — descend: expand a repository or directory, move focus at the pane
    /// edges, zoom a file.
    pub fn open(&mut self) {
        // Driving a document: descending means unfolding the selected row, and
        // there is nothing else left to open. Unfolding only — `→` on something
        // already open does nothing, as it does on an open directory in the
        // tree. `←` folds, and `space` toggles.
        if self.driving_doc() {
            if let Some(doc) = self.focused_doc_mut() {
                doc.expand_cursor();
            }
            return;
        }
        match self.focus {
            // Taken by `driving_doc` above, which is the only way focus is here.
            Focus::Preview => {}
            Focus::Repos => {
                let Some(row) = self.repos.selected_row() else {
                    return;
                };
                let (repo, is_ref, expanded, expandable) = (
                    row.repo.clone(),
                    row.reference.is_some(),
                    row.expanded,
                    row.expandable,
                );
                if is_ref || expanded || !expandable {
                    // Nothing further to open in pane one — step into the tree.
                    self.focus = Focus::Tree;
                    self.mark_preview_dirty();
                } else {
                    // Its refs may hold only the default branch, which is not
                    // listed; the reply carries the descent on if so.
                    self.descend_after_refs = Some(repo.clone());
                    self.expand_repo(&repo);
                }
            }
            Focus::Tree => {
                let Some(slot) = self.tree.selected_slot() else {
                    return;
                };
                if !self.tree.nodes[slot].is_dir() {
                    // A `.json` in the preview is a document of its own, so the
                    // tree's right edge is a step into it — the same move `→`
                    // makes from pane one into the tree.
                    if self.preview_folds() {
                        self.focus = Focus::Preview;
                        return;
                    }
                    // Anything else has no level to step into, and filling the
                    // screen with it is a bigger step than `→` takes anywhere
                    // else, so it belongs to `Enter` alone — see [`App::enter`].
                    // Said rather than done quietly: the key you walked the tree
                    // down with stopping without a word reads like a broken row.
                    let name = self.tree.nodes[slot].name.clone();
                    self.set_status(format!("⏎ opens {name}"), false);
                    return;
                }
                self.tree.nodes[slot].expanded = true;
                match self.tree.nodes[slot].load {
                    // Retry a directory whose listing failed.
                    Load::Idle | Load::Failed(_) => self.load_children(slot),
                    Load::Loading => {}
                    Load::Ready => self.tree.rebuild_rows(),
                }
            }
        }
    }

    /// `⏎` — the same descent as `→` everywhere except on a file, which it and
    /// it alone opens full-screen.
    ///
    /// Zooming is not a step of the same size as the others: it replaces the
    /// three panes with one file, and `Esc` is the only way back out. A key held
    /// down to walk the tree should not fall into that at the first leaf it
    /// meets, so the deliberate key gets it and `→` stays a tree movement.
    pub fn enter(&mut self) {
        if self.zoomed() {
            self.open();
            return;
        }
        // From the tree, on a file. From the preview, whatever it is reading —
        // the cursor is kept, so the zoom opens on the row you were on and `Esc`
        // puts you back in the pane you left.
        let opens = match self.focus {
            Focus::Tree => self.tree.selected().is_some_and(|n| !n.is_dir()),
            Focus::Preview => true,
            Focus::Repos => false,
        };
        if opens {
            self.mode = Mode::Zoom;
            self.preview.scroll = 0;
            return;
        }
        self.open();
    }

    /// Leave a zoom, answering whether there was one to leave so the caller can
    /// fall back to whatever its key otherwise means.
    ///
    /// `Esc` and `Backspace` both come through here. Neither is a key held down to
    /// wind a document shut — that is `←`'s job, and why `←` stops at the last
    /// fold instead of dropping out of the file with it.
    pub fn leave_zoom(&mut self) -> bool {
        if self.mode == Mode::Zoom {
            self.mode = Mode::Normal;
            return true;
        }
        false
    }

    /// Step out of the preview back to the tree, answering whether the focus was
    /// there to move. `Esc` and `Backspace`, which leave in one press rather than
    /// folding their way out the way `←` does.
    pub fn leave_preview(&mut self) -> bool {
        if self.focus == Focus::Preview {
            self.focus = Focus::Tree;
            return true;
        }
        false
    }

    /// `←` — ascend: collapse, step to the parent, or move focus left.
    pub fn back(&mut self) {
        if self.zoomed() {
            // A zoomed document folds its way back up a level at a time. Once
            // nothing is left open there is nothing here to do: leaving is
            // `Esc`'s alone, so winding a file all the way closed doesn't also
            // throw away the file you were reading.
            if let Some(doc) = self.focused_doc_mut() {
                doc.back();
            }
            return;
        }
        match self.focus {
            // The same ascent every other pane makes: fold what is open, step
            // out of what is not, and once nothing is left move focus left. The
            // zoom stops short of that last step because leaving it costs you
            // the file; here the file is still right there in the pane.
            Focus::Preview => {
                let folded = self.focused_doc_mut().is_some_and(|doc| doc.back());
                if !folded {
                    self.focus = Focus::Tree;
                }
            }
            Focus::Repos => {
                let Some(row) = self.repos.selected_row() else {
                    return;
                };
                let repo = row.repo.clone();
                if row.reference.is_some() || row.expanded {
                    self.collapse_repo(&repo);
                }
            }
            Focus::Tree => {
                let Some(slot) = self.tree.selected_slot() else {
                    self.focus = Focus::Repos;
                    self.mark_preview_dirty();
                    return;
                };
                let node = &self.tree.nodes[slot];
                if node.is_dir() && node.expanded && self.tree.filter.is_empty() {
                    self.tree.nodes[slot].expanded = false;
                    self.tree.rebuild_rows();
                    return;
                }
                match node.parent.filter(|p| *p != ROOT) {
                    Some(parent) => {
                        self.tree.select_slot(parent);
                        self.mark_preview_dirty();
                    }
                    None => {
                        self.focus = Focus::Repos;
                        self.mark_preview_dirty();
                    }
                }
            }
        }
    }

    /// `space` — expand or collapse in place, without moving focus.
    pub fn toggle(&mut self) {
        if self.driving_doc() {
            if let Some(doc) = self.focused_doc_mut() {
                doc.toggle_cursor();
            }
            return;
        }
        match self.focus {
            // Not a list of rows, and `driving_doc` above has already had it.
            Focus::Preview => {}
            Focus::Repos => {
                let Some(row) = self.repos.selected_row() else {
                    return;
                };
                let (repo, expanded) = (row.repo.clone(), row.expanded);
                if row.reference.is_some() || !row.expandable {
                    return;
                }
                if expanded {
                    self.collapse_repo(&repo);
                } else {
                    self.expand_repo(&repo);
                }
            }
            Focus::Tree => {
                let Some(slot) = self.tree.selected_slot() else {
                    return;
                };
                if !self.tree.nodes[slot].is_dir() {
                    return;
                }
                if self.tree.nodes[slot].expanded {
                    self.tree.nodes[slot].expanded = false;
                    self.tree.rebuild_rows();
                } else {
                    self.open();
                }
            }
        }
    }

    /// `a` — unfold the whole zoomed document: all of it when the cursor is on a
    /// folded record, else to the level that record is reading at, from wherever
    /// inside it the cursor sits.
    ///
    /// A level is otherwise invisible — a row folded at level 2 looks like one
    /// folded at level 5 — so it is worth saying which one you got.
    pub fn expand_all(&mut self) {
        let Some(doc) = self.focused_doc_mut() else {
            return;
        };
        match doc.expand_all() {
            Some(depth) => self.set_status(format!("unfolded everything to level {depth}"), false),
            None => self.set_status("unfolded everything", false),
        }
    }

    /// `c` — fold the whole zoomed document back up.
    pub fn collapse_all(&mut self) {
        let Some(doc) = self.focused_doc_mut() else {
            return;
        };
        doc.collapse_all();
        self.set_status("folded everything", false);
    }

    // ── the key filter ───────────────────────────────────────────────────

    /// `F` — open the menu of keys the zoomed records use.
    ///
    /// Only JSONL has one. A whole JSON file is a single value rather than a
    /// shape repeated, so there is no set of keys that switching off would say
    /// anything about.
    pub fn open_keys(&mut self) {
        let Some(doc) = self.jsonl() else {
            self.set_status("F filters the keys of a zoomed .jsonl file", true);
            return;
        };
        if doc.keys().is_empty() {
            self.set_status("nothing to filter: these records have no object keys", true);
            return;
        }
        // The menu opens in the shape of the record under the cursor: the keys
        // that record has unfolded are the keys it lists the contents of.
        let entry = doc.cursor_entry();
        self.keys.undo = Some(doc.keys().clone());
        self.keys.edited = false;
        self.mode = Mode::Keys;
        self.status = None;
        if let (Some(doc), Some(entry)) = (self.jsonl_mut(), entry) {
            doc.open_keys_to(entry);
        }
        self.clamp_keys_cursor();
    }

    /// Close the menu, keeping the edits or putting the old filter back.
    pub fn close_keys(&mut self, keep: bool) {
        // Back to the zoom the menu was opened over, not to the tree.
        self.mode = Mode::Zoom;
        let undo = self.keys.undo.take();
        let edited = std::mem::take(&mut self.keys.edited);
        if let (false, true, Some(old)) = (keep, edited, undo)
            && let Some(doc) = self.jsonl_mut()
        {
            doc.edit_keys(|filter| *filter = old);
        }
    }

    pub fn keys_rows(&self) -> Vec<MenuRow> {
        self.jsonl()
            .map(|doc| doc.keys().rows())
            .unwrap_or_default()
    }

    /// The tree node the menu selection is on.
    fn keys_path(&self) -> Option<Vec<usize>> {
        self.keys_rows()
            .into_iter()
            .nth(self.keys.cursor)
            .map(|row| row.path)
    }

    pub fn keys_move(&mut self, delta: isize) {
        let last = self.keys_rows().len() as isize - 1;
        if last < 0 {
            return;
        }
        self.keys.cursor = (self.keys.cursor as isize)
            .saturating_add(delta)
            .clamp(0, last) as usize;
        self.focus_keys_cursor();
    }

    /// Switch the selected key on or off.
    pub fn keys_toggle(&mut self) {
        let Some(path) = self.keys_path() else {
            return;
        };
        if let Some(doc) = self.jsonl_mut() {
            doc.edit_keys(|filter| filter.toggle(&path));
        }
        self.keys.edited = true;
    }

    /// Unfold or fold the selected key. Folding one that is already folded steps
    /// out to its parent, so `←` walks back up the tree the way it does
    /// everywhere else.
    pub fn keys_fold(&mut self, open: bool) {
        let Some(path) = self.keys_path() else {
            return;
        };
        let moved = self
            .jsonl_mut()
            .is_some_and(|doc| doc.fold_keys(&path, open));
        if moved {
            self.fold_zoom_to_keys();
            self.clamp_keys_cursor();
            return;
        }
        if !open && path.len() > 1 {
            let parent = &path[..path.len() - 1];
            if let Some(row) = self.keys_rows().iter().position(|r| r.path == parent) {
                self.keys.cursor = row;
                self.focus_keys_cursor();
            }
        }
    }

    /// Bring the record being read to the shape the menu is now unfolded to, so
    /// the key being switched is a key on show behind it. The other half of the
    /// sync `open_keys` starts: the menu follows the record it is opened over,
    /// and that record follows the menu from then on.
    ///
    /// Only that one record. The rest of the file is left as it was — opening
    /// the whole of it is what `a` is for, and it is not what somebody unfolding
    /// a key in the menu asked for.
    fn fold_zoom_to_keys(&mut self) {
        let Some(doc) = self.jsonl_mut() else {
            return;
        };
        let Some(entry) = doc.cursor_entry() else {
            return;
        };
        doc.open_entry_to_keys(entry);
    }

    pub fn keys_set_all(&mut self, enabled: bool) {
        if let Some(doc) = self.jsonl_mut() {
            doc.edit_keys(|filter| filter.set_all(enabled));
        }
        self.keys.edited = true;
    }

    /// Select a menu line, as a click does.
    pub fn keys_select(&mut self, row: usize) {
        if row < self.keys_rows().len() {
            self.keys.cursor = row;
            self.focus_keys_cursor();
        }
    }

    /// The wheel scrolls the menu, dragging the selection along by its edge so
    /// it can't be left off screen.
    pub fn keys_scroll(&mut self, down: bool) {
        let rows = self.keys_rows().len();
        let max = rows.saturating_sub(self.keys.viewport.max(1));
        self.keys.scroll = if down {
            (self.keys.scroll + WHEEL_LINES).min(max)
        } else {
            self.keys.scroll.saturating_sub(WHEEL_LINES)
        };
        let bottom = self.keys.scroll + self.keys.viewport.max(1) - 1;
        self.keys.cursor = self
            .keys
            .cursor
            .clamp(self.keys.scroll.min(bottom), bottom)
            .min(rows.saturating_sub(1));
    }

    /// Note how tall the menu came out, keeping the selection in view.
    pub fn keys_resize(&mut self, viewport: usize) {
        self.keys.viewport = viewport.max(1);
        self.focus_keys_cursor();
    }

    /// Whether a click landed on the menu at all, its border included.
    pub fn in_keys_popup(&self, col: u16, row: u16) -> bool {
        Hits::hit(self.keys.popup, col, row)
    }

    /// Map a click to a menu line.
    pub fn keys_row_at(&self, col: u16, row: u16) -> Option<usize> {
        if !Hits::hit(self.keys.list, col, row) {
            return None;
        }
        let line = self.keys.scroll + (row - self.keys.list.y) as usize;
        (line < self.keys_rows().len()).then_some(line)
    }

    fn clamp_keys_cursor(&mut self) {
        let last = self.keys_rows().len().saturating_sub(1);
        self.keys.cursor = self.keys.cursor.min(last);
        self.focus_keys_cursor();
    }

    fn focus_keys_cursor(&mut self) {
        let viewport = self.keys.viewport.max(1);
        if self.keys.cursor < self.keys.scroll {
            self.keys.scroll = self.keys.cursor;
        } else if self.keys.cursor >= self.keys.scroll + viewport {
            self.keys.scroll = self.keys.cursor + 1 - viewport;
        }
        // Never leave blank rows under the last key. The menu is sized to its
        // own content on each draw, so a scroll that was needed a moment ago —
        // before the first draw said how tall the list is, or before unfolding
        // gave it more room — is usually not needed now.
        let max = self.keys_rows().len().saturating_sub(viewport);
        self.keys.scroll = self.keys.scroll.min(max);
    }

    // ── tabs ─────────────────────────────────────────────────────────────

    pub fn select_tab(&mut self, tab: Tab) {
        self.tab = tab;
        // Zoom is a Browse-tab thing; leaving the tab leaves the zoom, but
        // asking for the tab you are already on changes nothing.
        if self.zoomed() && tab != Tab::Browse {
            self.mode = Mode::Normal;
        }
        if tab == Tab::Commits {
            if self.context().is_none() {
                self.set_status("open a repository and ref in Browse first", false);
            }
            self.load_commits(false);
        }
    }

    // ── filter ───────────────────────────────────────────────────────────

    /// `/` — start typing into the focused pane's filter, if it has one.
    ///
    /// The preview has none: it is one file rather than a list, and letting the
    /// mode open there would swallow every keystroke into a search that could
    /// never match anything.
    pub fn start_filter(&mut self) {
        if self.focus == Focus::Preview {
            self.set_status(
                "nothing to search in a file — ← goes back to the tree",
                false,
            );
            return;
        }
        self.mode = Mode::Filter;
    }

    fn refilter(&mut self) {
        match self.focus {
            Focus::Preview => {}
            Focus::Repos => {
                self.repos.rebuild();
                self.sync_target();
            }
            Focus::Tree => {
                // Re-mark immediately so already-loaded nodes respond to every
                // keystroke; only the network crawl waits for a pause.
                self.tree.mark_matches();
                self.tree.rebuild_rows();
                if self.tree.filter.is_empty() {
                    self.tree.filter_dirty = None;
                    self.tree.capped = false;
                    self.bump_crawl();
                } else {
                    self.tree.filter_dirty = Some(Instant::now());
                }
            }
        }
        self.mark_preview_dirty();
    }

    pub fn filter_push(&mut self, c: char) {
        match self.focus {
            Focus::Repos => self.repos.filter.push(c),
            Focus::Tree => self.tree.filter.push(c),
            Focus::Preview => return,
        }
        self.refilter();
    }

    pub fn filter_pop(&mut self) {
        match self.focus {
            Focus::Repos => self.repos.filter.pop(),
            Focus::Tree => self.tree.filter.pop(),
            Focus::Preview => return,
        };
        self.refilter();
    }

    pub fn filter_clear(&mut self) {
        match self.focus {
            Focus::Repos => self.repos.filter.clear(),
            Focus::Tree => self.tree.filter.clear(),
            Focus::Preview => return,
        }
        self.refilter();
    }

    // ── profiles ─────────────────────────────────────────────────────────

    pub fn profile_names(&self) -> Vec<String> {
        self.cfg.profiles.keys().cloned().collect()
    }

    pub fn switch_profile(&mut self, name: &str) {
        let Some(profile) = self.cfg.profiles.get(name).cloned() else {
            return;
        };
        match Client::new(&profile, self.cfg.ui.page_size) {
            Ok(client) => {
                self.client = client;
                self.profile = profile;
                self.profile_name = name.to_string();
                self.commits = CommitsView::default();
                self.repos = ReposView::default();
                self.focus = Focus::Repos;
                self.pending_jump = None;
                self.descend_after_refs = None;
                self.pending_expand.clear();
                self.set_target(None);
                self.load_repos();
                self.set_status(format!("switched to profile `{name}`"), false);
            }
            Err(e) => self.set_status(format!("{name}: {e}"), true),
        }
    }

    /// Jump straight to `repo` (and optionally `reference`) on start-up. The
    /// tree loads at once; pane one catches up when the repository list lands.
    pub fn open_path(&mut self, repo: &str, reference: Option<&str>) {
        self.pending_jump = Some((repo.to_string(), reference.map(String::from)));
        if let Some(reference) = reference {
            self.set_target(Some((repo.to_string(), reference.to_string())));
            self.focus = Focus::Tree;
        }
    }
}

fn fmt_err(e: anyhow::Error) -> String {
    // Flatten the anyhow chain into one line, dropping duplicate context.
    let mut parts: Vec<String> = Vec::new();
    for cause in e.chain() {
        let s = cause.to_string();
        if !parts.iter().any(|p| p == &s) {
            parts.push(s);
        }
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::test_support::{stat, test_app};

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

    // ── the zoom is `⏎`'s to enter and `Esc`'s to leave ───────────────────

    /// A tree of one directory and one file, focused, with the file selected.
    fn app_on_a_file() -> App {
        let mut app = test_app();
        app.tree.key = Some(("repo".into(), "main".into()));
        app.on_msg(Msg::Children {
            generation: app.tree.generation,
            prefix: String::new(),
            res: Ok(vec![stat("data/", true), stat("notes.txt", false)]),
        });
        app.focus = Focus::Tree;
        // Directories sort first, so the file is the second row.
        app.tree.state.select(Some(1));
        assert!(app.tree.selected().is_some_and(|n| !n.is_dir()));
        app.status = None;
        app
    }

    /// `→` walks the tree and stops at a file. It used to fill the screen with
    /// it, which a key you hold down should not be able to do.
    #[tokio::test]
    async fn right_on_a_file_does_not_zoom() {
        let mut app = app_on_a_file();
        app.open();
        assert_eq!(app.mode, Mode::Normal, "`→` opened the zoom");
        // And it names the key that does, rather than reading as a dead row.
        let status = app.status.as_ref().expect("`→` on a file said nothing");
        assert!(status.text.contains("notes.txt"), "{}", status.text);
        assert!(!status.is_error, "declining to zoom is not a failure");
    }

    #[tokio::test]
    async fn enter_on_a_file_zooms_it() {
        let mut app = app_on_a_file();
        app.preview.scroll = 7;
        app.enter();
        assert_eq!(app.mode, Mode::Zoom);
        assert_eq!(app.preview.scroll, 0, "the zoom opens at the top");
    }

    /// Everywhere but a file, `⏎` is `→`: a directory opens rather than zooming.
    #[tokio::test]
    async fn enter_on_a_directory_descends_like_right() {
        let mut app = app_on_a_file();
        app.tree.state.select(Some(0));
        app.enter();
        assert_eq!(app.mode, Mode::Normal, "a directory is not a file");
        assert!(app.tree.nodes[app.tree.rows[0]].expanded);
    }

    /// `←` winds a document shut and then stops. Folding the last block used to
    /// drop out of the zoom along with it, so a held `h` could lose you the file
    /// you were reading.
    #[tokio::test]
    async fn left_in_a_zoom_folds_but_never_leaves() {
        let mut app = app_on_a_file();
        let value: serde_json::Value = serde_json::from_str(r#"{"a":{"b":1}}"#).unwrap();
        let mut doc = crate::jsonl::JsonDoc::new(value);
        // Resting inside `"a"`, so `←` has somewhere to wind out of.
        doc.cursor = 2;
        app.preview.body = Some(PreviewBody::Json(doc));
        app.enter();
        assert_eq!(app.mode, Mode::Zoom);
        let open = app.focused_doc().unwrap().rows_len();

        // Out to `"a"`, which folds; after that there is nowhere left to go, and
        // none of the presses leaves.
        for _ in 0..5 {
            app.back();
            assert_eq!(app.mode, Mode::Zoom, "`←` left the zoom");
        }
        let shut = app.focused_doc().unwrap().rows_len();
        assert!(shut < open, "`←` folded nothing: {shut} of {open} rows");

        // `Esc` and `Backspace` are the way out.
        assert!(app.leave_zoom());
        assert!(!app.zoomed());
    }

    /// `Esc` and `Backspace` share this, and each falls back to its own meaning
    /// when there was no zoom: clearing the search, and stepping back up the tree.
    #[tokio::test]
    async fn leaving_a_zoom_says_whether_there_was_one() {
        let mut app = app_on_a_file();
        assert!(!app.leave_zoom(), "nothing was zoomed");

        app.enter();
        assert_eq!(app.mode, Mode::Zoom);
        assert!(app.leave_zoom(), "the zoom went unreported");
        assert_eq!(app.mode, Mode::Normal);
        assert!(!app.leave_zoom(), "left twice");
    }

    // ── folds outlive the preview holding them ───────────────────────────

    const OBJECT: &[u8] = br#"{"a":1,"b":{"c":2}}"#;

    fn key(path: &str) -> (String, String, String) {
        ("repo".into(), "main".into(), path.into())
    }

    /// Land a fetched body on the preview the way the reply does, under `key`.
    fn arrive(app: &mut App, key: (String, String, String), path: &str, bytes: &[u8]) {
        app.preview.key = Some(key);
        app.preview.req = app.req_id();
        let req = app.preview.req;
        app.on_msg(Msg::Preview(
            req,
            Ok(PreviewPayload {
                stat: stat(path, false),
                bytes: bytes.to_vec(),
                truncated: false,
            }),
        ));
    }

    /// Rows on show, which is how much of the document is folded.
    fn shown(app: &mut App) -> usize {
        app.json_doc_mut().expect("a JSON preview").rows_len()
    }

    /// A preview lasts only as long as the selection sits on it, so without a
    /// memory of its own every fold would be undone by a glance at the file next
    /// door.
    #[tokio::test]
    async fn a_json_file_comes_back_folded_the_way_it_was_left() {
        let mut app = test_app();
        arrive(&mut app, key("a.json"), "a.json", OBJECT);

        let open = shown(&mut app);
        // Row 2 is `▾ "b": {` — row 0 is the file's own brace, row 1 is `"a"`.
        app.json_doc_mut().unwrap().toggle_row(2);
        let folded = shown(&mut app);
        assert!(folded < open, "nothing folded: {folded} of {open} rows");

        // Away, through the clearing a directory or a lost ref does...
        app.clear_preview();
        assert!(app.preview.body.is_none());
        // ...and back, refetched from scratch: it is put back as it was.
        arrive(&mut app, key("a.json"), "a.json", OBJECT);
        assert_eq!(shown(&mut app), folded, "the folds were forgotten");
    }

    /// The shape belongs to the object, not to the pane: another file arriving
    /// in between neither loses it nor inherits it.
    #[tokio::test]
    async fn one_file_s_folds_are_not_another_s() {
        let mut app = test_app();
        arrive(&mut app, key("a.json"), "a.json", OBJECT);
        let open = shown(&mut app);
        app.json_doc_mut().unwrap().toggle_row(2);
        let folded = shown(&mut app);

        app.clear_preview();
        arrive(&mut app, key("b.json"), "b.json", OBJECT);
        assert_eq!(shown(&mut app), open, "b.json opened with a.json's folds");

        app.clear_preview();
        arrive(&mut app, key("a.json"), "a.json", OBJECT);
        assert_eq!(shown(&mut app), folded);
    }

    /// Reading past the cap drops the oldest shapes rather than growing for the
    /// length of the session.
    #[tokio::test]
    async fn only_so_many_fold_shapes_are_kept() {
        let mut app = test_app();
        for i in 0..FOLD_MEMORY + 4 {
            arrive(&mut app, key(&format!("{i}.json")), "n.json", OBJECT);
            app.json_doc_mut().unwrap().toggle_row(2);
            app.clear_preview();
        }
        assert_eq!(app.folds.len(), FOLD_MEMORY);
        // The oldest went; the newest stayed.
        assert!(app.folds.iter().all(|(k, _)| k.2 != "0.json"));
        let last = key(&format!("{}.json", FOLD_MEMORY + 3));
        assert!(app.folds.iter().any(|(k, _)| *k == last));
    }

    /// `.jsonl` is left as it was: its records fold in the zoom alone, and that
    /// state still goes with the preview.
    #[tokio::test]
    async fn a_jsonl_file_keeps_nothing() {
        let mut app = test_app();
        let records = b"{\"a\":1}\n{\"a\":2}\n";
        arrive(&mut app, key("a.jsonl"), "a.jsonl", records);
        app.mode = Mode::Zoom;
        app.focused_doc_mut().unwrap().toggle_row(0);
        assert!(
            app.focused_doc().unwrap().rows_len() > 2,
            "the record opened"
        );

        app.clear_preview();
        assert!(app.folds.is_empty(), "a .jsonl left a shape behind");
        arrive(&mut app, key("a.jsonl"), "a.jsonl", records);
        assert_eq!(
            app.focused_doc().unwrap().rows_len(),
            2,
            "the records came back open"
        );
    }

    // ── the cursor steps into the preview ────────────────────────────────

    /// A tree on one file, with `body` previewed under it.
    fn app_reading(path: &str, body: &[u8]) -> App {
        let mut app = test_app();
        app.tree.key = Some(("repo".into(), "main".into()));
        app.on_msg(Msg::Children {
            generation: app.tree.generation,
            prefix: String::new(),
            res: Ok(vec![stat(path, false)]),
        });
        app.focus = Focus::Tree;
        app.tree.state.select(Some(0));
        arrive(&mut app, key(path), path, body);
        app.status = None;
        app
    }

    /// `→` at the tree's right edge steps into the preview, the same move it
    /// makes from pane one into the tree — but only where there is a document
    /// for the cursor to drive.
    #[tokio::test]
    async fn right_steps_into_a_json_preview_and_nothing_else() {
        let mut app = app_reading("a.json", OBJECT);
        app.open();
        assert_eq!(app.focus, Focus::Preview);
        assert_eq!(app.mode, Mode::Normal, "`→` zoomed");
        assert!(app.status.is_none(), "`→` had something to say");

        // A `.jsonl` folds in the zoom alone, so the tree keeps the keys and `→`
        // says what does open it.
        let mut app = app_reading("a.jsonl", b"{\"a\":1}\n");
        app.open();
        assert_eq!(app.focus, Focus::Tree);
        assert!(
            app.status
                .as_ref()
                .is_some_and(|s| s.text.contains("a.jsonl")),
            "{:?}",
            app.status.as_ref().map(|s| &s.text)
        );

        // And so does plain text.
        let mut app = app_reading("a.txt", b"hello\n");
        app.open();
        assert_eq!(app.focus, Focus::Tree);
    }

    /// `Esc` and `Backspace` leave the pane in one press, without folding their
    /// way out the way `←` does.
    #[tokio::test]
    async fn escape_leaves_the_preview_in_one_press() {
        let mut app = app_reading("a.json", OBJECT);
        assert!(!app.leave_preview(), "the tree had the keys");

        app.open();
        let open = shown(&mut app);
        assert!(app.leave_preview());
        assert_eq!(app.focus, Focus::Tree);
        assert_eq!(shown(&mut app), open, "leaving folded something");
        assert!(!app.leave_preview(), "left twice");
    }

    /// The cursor must not sit in a pane it cannot drive. Moving off the object
    /// takes the preview with it, and a reload can land something with nothing
    /// to fold.
    #[tokio::test]
    async fn the_keys_go_back_to_the_tree_when_the_document_does() {
        let mut app = app_reading("a.json", OBJECT);
        app.open();
        assert_eq!(app.focus, Focus::Preview);
        app.clear_preview();
        assert_eq!(app.focus, Focus::Tree, "the cursor stayed in an empty pane");

        let mut app = app_reading("a.json", OBJECT);
        app.open();
        assert_eq!(app.focus, Focus::Preview);
        // The same object, rewritten as something that does not parse.
        arrive(&mut app, key("a.json"), "a.json", b"not json at all");
        assert_eq!(app.focus, Focus::Tree, "the cursor stayed on a text body");
    }

    /// The focus outlives a change of tab, so that coming back to Browse puts
    /// you where you left off — which means the document keys have to ask which
    /// tab they are on before folding a file nobody can see.
    #[tokio::test]
    async fn the_document_keys_are_the_browse_tab_s_alone() {
        let mut app = app_reading("a.json", OBJECT);
        app.open();
        assert_eq!(app.focus, Focus::Preview);
        let open = shown(&mut app);

        app.tab = Tab::Commits;
        app.collapse_all();
        app.select_edge(false);
        app.move_selection(3);
        assert_eq!(shown(&mut app), open, "the commit log folded the preview");

        // Back on Browse the cursor is where it was, and the keys work again.
        app.tab = Tab::Browse;
        app.collapse_all();
        assert!(shown(&mut app) < open, "the preview stopped folding");
    }

    /// The preview is one file, not a list, so `/` has nothing to search there —
    /// and must not open a mode that would swallow every keystroke.
    #[tokio::test]
    async fn the_preview_has_no_search() {
        let mut app = app_reading("a.json", OBJECT);
        app.open();
        app.start_filter();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.status.is_some(), "`/` said nothing");

        app.leave_preview();
        app.start_filter();
        assert_eq!(app.mode, Mode::Filter, "the tree lost its search");
    }
}
