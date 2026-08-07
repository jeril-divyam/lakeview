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
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{Config, Profile};
use crate::lakefs::{Client, Commit, NamedRef, ObjectStats, RefKind, Repository};

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
}

pub struct PreviewPayload {
    pub stat: ObjectStats,
    pub bytes: Vec<u8>,
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
    /// A repository row whose refs are still in flight.
    pub loading: bool,
}

#[derive(Default)]
pub struct ReposView {
    pub repos: Vec<Repository>,
    pub refs: HashMap<String, RefsSlot>,
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
                loading: matches!(slot.map(|s| &s.load), Some(Load::Loading)),
            });
            if !expanded {
                continue;
            }
            let Some(slot) = slot else { continue };
            for r in &slot.refs {
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
                physical_address: String::new(),
                checksum: String::new(),
                size_bytes: None,
                mtime: 0,
                content_type: None,
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
    /// Re-indented JSON, tokenised so the UI can colour it.
    Json(Vec<JsonLine>),
    Binary(Vec<String>),
}

/// One rendered line of JSON as a run of (token kind, text) pairs.
pub type JsonLine = Vec<(JsonTok, String)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonTok {
    Key,
    Str,
    Num,
    Bool,
    Null,
    Punct,
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
}

pub struct Status {
    pub text: String,
    pub is_error: bool,
    pub at: Instant,
}

// ── mouse hit-testing ────────────────────────────────────────────────────

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
    /// Inner area of the commit list.
    pub commits: Option<Rect>,
    /// (tab, label area) for each tab in the header.
    pub tabs: Vec<(Tab, Rect)>,
}

impl Hits {
    fn hit(area: Rect, col: u16, row: u16) -> bool {
        col >= area.x && col < area.x + area.width && row >= area.y && row < area.y + area.height
    }
}

/// How many lines one wheel notch moves.
const WHEEL_LINES: usize = 3;
/// Two clicks on the same cell within this window count as a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

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
    /// Directories a reload should re-open, applied as each level lands.
    pending_expand: Vec<String>,
    /// Set when the pane-one selection moved; the tree follows once it settles.
    target_dirty: Option<Instant>,
    /// Regions recorded by the last render, for mouse hit-testing.
    pub hits: Hits,
    /// Cell and time of the last click, used to detect double-clicks.
    last_click: Option<(u16, u16, Instant)>,
}

impl App {
    pub fn new(
        cfg: Config,
        profile_name: String,
        profile: Profile,
        client: Client,
        tx: UnboundedSender<Msg>,
    ) -> Self {
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
            status: None,
            should_quit: false,
            next_req: 0,
            inflight: 0,
            tick: 0,
            crawl_token: Arc::new(AtomicU64::new(0)),
            pending_jump: None,
            pending_expand: Vec::new(),
            target_dirty: None,
            hits: Hits::default(),
            last_click: None,
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

    pub fn breadcrumb(&self) -> Vec<String> {
        let mut parts = Vec::new();
        let Some((repo, reference)) = &self.tree.key else {
            return parts;
        };
        parts.push(repo.clone());
        parts.push(reference.clone());
        if self.focus == Focus::Tree
            && let Some(node) = self.tree.selected()
        {
            parts.extend(
                node.stat
                    .path
                    .trim_end_matches('/')
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .map(String::from),
            );
        }
        parts
    }

    /// The filter belonging to whichever pane has focus.
    pub fn filter(&self) -> &str {
        match self.focus {
            Focus::Repos => &self.repos.filter,
            Focus::Tree => &self.tree.filter,
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
        let (tx, client, repo) = (self.tx.clone(), self.client.clone(), repo.to_string());
        tokio::spawn(async move {
            let res = client.refs(&repo, show_tags).await.map_err(fmt_err);
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
                self.load_repos();
                self.set_status("reloading repositories…", false);
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
        // Only the tree selects objects; pane one renders its own details.
        let object = match self.focus {
            Focus::Repos => None,
            Focus::Tree => self.tree.selected().map(|n| n.stat.clone()),
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
                .map(|bytes| PreviewPayload {
                    stat: object,
                    bytes,
                })
                .map_err(fmt_err);
            let _ = tx.send(Msg::Preview(req, res));
        });
    }

    fn clear_preview(&mut self) {
        self.preview = Preview::default();
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
                match res {
                    Ok(refs) => {
                        if let Some(slot) = self.repos.refs.get_mut(&repo) {
                            slot.refs = refs;
                            slot.load = Load::Ready;
                        }
                        self.repos.rebuild();
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
                        self.preview.body = Some(render_body(&payload.bytes));
                        self.preview.stat = Some(payload.stat);
                    }
                    Err(e) => self.preview.error = Some(e),
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
        match self
            .repos
            .rows
            .iter()
            .position(|r| r.repo == repo && r.reference.as_deref() == Some(reference.as_str()))
        {
            Some(row) => {
                self.repos.state.select(Some(row));
                self.sync_target();
            }
            None => self.set_status(format!("ref `{reference}` not found in `{repo}`"), true),
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
            _ if self.mode == Mode::Zoom => {
                self.preview.scroll = self
                    .preview
                    .scroll
                    .saturating_add_signed(delta.clamp(-1000, 1000) as i16);
            }
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
            },
        }
    }

    pub fn select_edge(&mut self, first: bool) {
        let (state, len) = match self.tab {
            Tab::Commits => (&mut self.commits.state, self.commits.commits.len()),
            _ => match self.focus {
                Focus::Repos => (&mut self.repos.state, self.repos.rows.len()),
                Focus::Tree => (&mut self.tree.state, self.tree.rows.len()),
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
        match self.focus {
            Focus::Repos => {
                let Some(row) = self.repos.selected_row() else {
                    return;
                };
                let (repo, is_ref, expanded) =
                    (row.repo.clone(), row.reference.is_some(), row.expanded);
                if is_ref || expanded {
                    // Nothing further to open in pane one — step into the tree.
                    self.focus = Focus::Tree;
                    self.mark_preview_dirty();
                } else {
                    self.expand_repo(&repo);
                }
            }
            Focus::Tree => {
                let Some(slot) = self.tree.selected_slot() else {
                    return;
                };
                if !self.tree.nodes[slot].is_dir() {
                    self.mode = Mode::Zoom;
                    self.preview.scroll = 0;
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

    /// `←` — ascend: collapse, step to the parent, or move focus left.
    pub fn back(&mut self) {
        if self.mode == Mode::Zoom {
            self.mode = Mode::Normal;
            return;
        }
        match self.focus {
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
        match self.focus {
            Focus::Repos => {
                let Some(row) = self.repos.selected_row() else {
                    return;
                };
                let (repo, expanded) = (row.repo.clone(), row.expanded);
                if row.reference.is_some() {
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

    /// Absolute path of the current selection, for the `y` (copy) action.
    pub fn selection_uri(&self) -> Option<String> {
        match self.focus {
            Focus::Repos => {
                let row = self.repos.selected_row()?;
                Some(match &row.reference {
                    Some(reference) => format!("lakefs://{}/{}", row.repo, reference),
                    None => format!("lakefs://{}", row.repo),
                })
            }
            Focus::Tree => {
                let (repo, reference) = self.tree.key.as_ref()?;
                let node = self.tree.selected()?;
                Some(format!("lakefs://{}/{}/{}", repo, reference, node.stat.path))
            }
        }
    }

    // ── tabs ─────────────────────────────────────────────────────────────

    pub fn select_tab(&mut self, tab: Tab) {
        self.tab = tab;
        if tab == Tab::Commits {
            if self.context().is_none() {
                self.set_status("open a repository and ref in Browse first", false);
            }
            self.load_commits(false);
        }
    }

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
        if self.mode == Mode::Zoom || self.hits.preview.is_some_and(|a| Hits::hit(a, col, row)) {
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

        if focus == self.focus {
            // The focused pane tracks the wheel like j/k, so the preview follows.
            self.move_selection(delta);
        } else {
            // The other pane just peeks: scroll the view, leave the selection
            // and the focus alone. Stop once the last row is on screen, so a
            // list shorter than its viewport doesn't scroll at all.
            let (state, len) = match focus {
                Focus::Repos => (&mut self.repos.state, self.repos.rows.len()),
                Focus::Tree => (&mut self.tree.state, self.tree.rows.len()),
            };
            let max = len.saturating_sub(area.height as usize);
            let offset = state.offset_mut();
            *offset = if down {
                (*offset + WHEEL_LINES).min(max)
            } else {
                offset.saturating_sub(WHEEL_LINES)
            };
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
            Focus::Tree => self.tree.state.select(Some(line)),
        }
        self.mark_preview_dirty();

        if double {
            // A container toggles, so the same gesture closes what it opened;
            // anything else opens — a ref steps right, a file zooms.
            let expandable = match self.focus {
                Focus::Repos => self
                    .repos
                    .selected_row()
                    .is_some_and(|r| r.reference.is_none()),
                Focus::Tree => self.tree.selected().is_some_and(|n| n.is_dir()),
            };
            if expandable {
                self.toggle();
            } else {
                self.open();
            }
        }
    }

    /// Right click mirrors `h`.
    pub fn mouse_back(&mut self) {
        self.back();
    }

    // ── filter ───────────────────────────────────────────────────────────

    fn refilter(&mut self) {
        match self.focus {
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
        }
        self.refilter();
    }

    pub fn filter_pop(&mut self) {
        match self.focus {
            Focus::Repos => self.repos.filter.pop(),
            Focus::Tree => self.tree.filter.pop(),
        };
        self.refilter();
    }

    pub fn filter_clear(&mut self) {
        match self.focus {
            Focus::Repos => self.repos.filter.clear(),
            Focus::Tree => self.tree.filter.clear(),
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

/// Decide whether the fetched bytes are text; fall back to a hex dump.
fn render_body(bytes: &[u8]) -> PreviewBody {
    let sample = &bytes[..bytes.len().min(8192)];
    let binary = sample.contains(&0)
        || String::from_utf8_lossy(sample)
            .chars()
            .filter(|c| *c == '\u{fffd}')
            .count()
            > 4;

    if binary {
        return PreviewBody::Binary(hex_dump(&bytes[..bytes.len().min(4096)]));
    }
    let text = String::from_utf8_lossy(bytes);

    // Pretty-print JSON. A body truncated by `preview_bytes` won't parse, so
    // this quietly falls through to the plain-text path.
    let head = text.trim_start();
    if (head.starts_with('{') || head.starts_with('['))
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let mut lines = Vec::new();
        write_json(&value, 0, None, false, &mut lines);
        return PreviewBody::Json(lines);
    }

    PreviewBody::Text(text.lines().map(|l| l.replace('\t', "    ")).collect())
}

/// Render `value` as indented, tokenised lines. `key` prefixes the value when
/// it sits inside an object; `comma` appends a separator.
fn write_json(
    value: &serde_json::Value,
    depth: usize,
    key: Option<&str>,
    comma: bool,
    out: &mut Vec<JsonLine>,
) {
    use serde_json::Value;

    let pad = "  ".repeat(depth);
    let mut line: JsonLine = vec![(JsonTok::Punct, pad.clone())];
    if let Some(key) = key {
        line.push((JsonTok::Key, quote(key)));
        line.push((JsonTok::Punct, ": ".to_string()));
    }

    // Open a container, recurse, then close it on its own line.
    let (open, close) = match value {
        Value::Object(map) if !map.is_empty() => ("{", "}"),
        Value::Array(items) if !items.is_empty() => ("[", "]"),
        _ => {
            let (tok, text) = match value {
                Value::Null => (JsonTok::Null, "null".to_string()),
                Value::Bool(b) => (JsonTok::Bool, b.to_string()),
                Value::Number(n) => (JsonTok::Num, n.to_string()),
                Value::String(s) => (JsonTok::Str, quote(s)),
                Value::Array(_) => (JsonTok::Punct, "[]".to_string()),
                Value::Object(_) => (JsonTok::Punct, "{}".to_string()),
            };
            line.push((tok, text));
            if comma {
                line.push((JsonTok::Punct, ",".to_string()));
            }
            out.push(line);
            return;
        }
    };

    line.push((JsonTok::Punct, open.to_string()));
    out.push(line);

    match value {
        Value::Object(map) => {
            let last = map.len() - 1;
            for (i, (k, v)) in map.iter().enumerate() {
                write_json(v, depth + 1, Some(k), i != last, out);
            }
        }
        Value::Array(items) => {
            let last = items.len() - 1;
            for (i, v) in items.iter().enumerate() {
                write_json(v, depth + 1, None, i != last, out);
            }
        }
        _ => unreachable!("only containers reach here"),
    }

    let mut tail = vec![(JsonTok::Punct, format!("{pad}{close}"))];
    if comma {
        tail.push((JsonTok::Punct, ",".to_string()));
    }
    out.push(tail);
}

/// Quote and escape a string the way JSON expects.
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

fn hex_dump(bytes: &[u8]) -> Vec<String> {
    bytes
        .chunks(16)
        .enumerate()
        .map(|(i, chunk)| {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            let ascii: String = chunk
                .iter()
                .map(|b| {
                    if b.is_ascii_graphic() || *b == b' ' {
                        *b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            format!("{:08x}  {:<47}  {}", i * 16, hex.join(" "), ascii)
        })
        .collect()
}
