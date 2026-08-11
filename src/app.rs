//! Application state and the update half of the loop.
//!
//! The browser is three fixed panes: repositories (expandable to reveal their
//! refs), a lazily-loaded tree of one ref's objects, and a detail/preview pane.
//! All network work happens off-thread and reports back through `Msg`; pane-one
//! requests carry a monotonic id and tree requests carry a generation, so stale
//! replies are dropped rather than applied to the wrong thing.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use futures::StreamExt;
use humansize::{DECIMAL, format_size};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{Config, Profile};
use crate::jsonl::Folding;
use crate::keys::{KeyFilter, MenuRow};
use crate::lakefs::{Client, Commit, NamedRef, ObjectStats, RefKind, Repository};
use crate::ui::{MIN_PREVIEW, MIN_REPOS, MIN_TREE, SCROLL_PADDING};

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
    /// Re-indented JSON, tokenised so the UI can colour it.
    ///
    /// Two renderings of one value: `lines` is the whole thing laid flat, which
    /// is all the side pane has room to say, and `doc` folds. Built once here
    /// rather than per frame, and bounded by `preview_bytes` either way.
    Json {
        lines: Vec<JsonLine>,
        doc: crate::jsonl::JsonDoc,
    },
    /// Newline-delimited JSON, one foldable record per row. The side pane still
    /// renders it as plain text; only the zoom unfolds it.
    Jsonl(crate::jsonl::Doc),
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
    /// The `▸` / `▾` a foldable JSONL row carries.
    Marker,
    /// A record that is not valid JSON, and the message saying so.
    Error,
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
    /// For a zoomed JSONL preview, the row each screen line of that area shows.
    /// A row that wrapped occupies several entries.
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
    fn hit(area: Rect, col: u16, row: u16) -> bool {
        col >= area.x && col < area.x + area.width && row >= area.y && row < area.y + area.height
    }
}

/// How many lines one wheel notch moves.
const WHEEL_LINES: usize = 3;
/// Two clicks on the same cell within this window count as a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// A pane border held by the mouse. The widths follow the pointer as it moves, so
/// this is only what the gesture itself has to remember.
#[derive(Debug, Clone, Copy)]
struct Drag {
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
}

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
    /// through `zoom_doc`.
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

    /// The foldable document the zoom is showing, of either kind.
    ///
    /// Folding is a zoom-only affair. The side pane is too narrow to read a
    /// folded row in — a JSON file gets its flat pretty-print there, and JSONL
    /// the plain lines it is.
    pub fn zoom_doc(&self) -> Option<&dyn Folding> {
        match (self.zoomed(), &self.preview.body) {
            (true, Some(PreviewBody::Jsonl(doc))) => Some(doc),
            (true, Some(PreviewBody::Json { doc, .. })) => Some(doc),
            _ => None,
        }
    }

    fn zoom_doc_mut(&mut self) -> Option<&mut dyn Folding> {
        match (self.zoomed(), &mut self.preview.body) {
            (true, Some(PreviewBody::Jsonl(doc))) => Some(doc),
            (true, Some(PreviewBody::Json { doc, .. })) => Some(doc),
            _ => None,
        }
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
                    truncated: bytes.len() as u64 >= limit,
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
                        self.preview.stat = Some(payload.stat);
                    }
                    Err(e) => self.preview.error = Some(e),
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
            // A zoomed JSONL preview has rows to move between; anything else
            // zoomed is a flat body, and moves the view itself.
            _ if self.zoomed() => match self.zoom_doc_mut() {
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
            },
        }
    }

    /// `Ctrl-f` / `Ctrl-b` — a screenful at a time through a zoomed foldable
    /// document. Zoom-only: the panes have `Ctrl-d` and `Ctrl-u`, and a flat
    /// body has no rows to page between.
    ///
    /// Where `Ctrl-d` moves the selection and leaves the view to chase it, this
    /// moves the view and brings the selection along to the top of it. The row
    /// straddling the edge is carried over whole rather than split between the
    /// two pages, so a page always starts where a row does — which is the same
    /// row of overlap you would get anywhere else.
    pub fn page(&mut self, down: bool) {
        let Some(last_row) = self.zoom_doc().map(|doc| doc.rows_len().saturating_sub(1)) else {
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
        if let Some(doc) = self.zoom_doc_mut() {
            doc.set_cursor(row);
        }
    }

    pub fn select_edge(&mut self, first: bool) {
        if self.zoomed() {
            match self.zoom_doc_mut() {
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
        // Already zoomed: descending means unfolding the selected row, and there
        // is nothing else left to open. Unfolding only — `→` on something
        // already open does nothing, as it does on an open directory in the
        // tree. `←` folds, and `space` toggles.
        if self.zoomed() {
            if let Some(doc) = self.zoom_doc_mut() {
                doc.expand_cursor();
            }
            return;
        }
        match self.focus {
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
                    // A file has no level to step into, and filling the screen
                    // with it is a bigger step than `→` takes anywhere else, so
                    // it belongs to `Enter` alone — see [`App::enter`]. Said
                    // rather than done quietly: the key you walked the tree down
                    // with stopping without a word reads like a broken row.
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
        if !self.zoomed()
            && self.focus == Focus::Tree
            && self.tree.selected().is_some_and(|n| !n.is_dir())
        {
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

    /// `←` — ascend: collapse, step to the parent, or move focus left.
    pub fn back(&mut self) {
        if self.zoomed() {
            // A zoomed document folds its way back up a level at a time. Once
            // nothing is left open there is nothing here to do: leaving is
            // `Esc`'s alone, so winding a file all the way closed doesn't also
            // throw away the file you were reading.
            if let Some(doc) = self.zoom_doc_mut() {
                doc.back();
            }
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
        if self.zoomed() {
            if let Some(doc) = self.zoom_doc_mut() {
                doc.toggle_cursor();
            }
            return;
        }
        match self.focus {
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
        let Some(doc) = self.zoom_doc_mut() else {
            return;
        };
        match doc.expand_all() {
            Some(depth) => self.set_status(format!("unfolded everything to level {depth}"), false),
            None => self.set_status("unfolded everything", false),
        }
    }

    /// `c` — fold the whole zoomed document back up.
    pub fn collapse_all(&mut self) {
        let Some(doc) = self.zoom_doc_mut() else {
            return;
        };
        doc.collapse_all();
        self.set_status("folded everything", false);
    }

    /// `d` — download the selected object into the working directory.
    ///
    /// The whole object, not the `preview_bytes` the preview settles for, and
    /// streamed to disk rather than held in memory. Nothing is dropped for being
    /// stale: the fetch is a side effect that outlives the selection that started
    /// it, and reports where it landed whenever it finishes.
    pub fn download_selected(&mut self) {
        let Some((repo, reference)) = self.context().map(|(r, f)| (r.to_string(), f.to_string()))
        else {
            self.set_status("open a repository and ref first", true);
            return;
        };
        if self.focus != Focus::Tree {
            self.set_status("select a file in the tree to download", true);
            return;
        }
        let Some(node) = self.tree.selected() else {
            self.set_status("nothing selected", true);
            return;
        };
        // A prefix is not one object; fetching a whole subtree is its own
        // feature, and silently downloading nothing would be worse than saying so.
        if node.is_dir() {
            self.set_status(
                format!("{}/ is a directory — d downloads a file", node.name),
                true,
            );
            return;
        }
        let (path, name) = (node.stat.path.clone(), node.name.clone());

        self.inflight += 1;
        self.set_status(format!("downloading {name}…"), false);

        let (tx, client) = (self.tx.clone(), self.client.clone());
        tokio::spawn(async move {
            let res = async {
                let (mut file, dest) = create_download_file(Path::new("."), &name).await?;
                let bytes = client
                    .download_object(&repo, &reference, &path, &mut file)
                    .await?;
                Ok((dest, bytes))
            }
            .await
            .map_err(fmt_err);
            let _ = tx.send(Msg::Download(res));
        });
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
        if self.zoomed() || self.hits.preview.is_some_and(|a| Hits::hit(a, col, row)) {
            if self.zoom_doc().is_some() {
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
        };
        if len == 0 || height == 0 {
            return;
        }
        let state = match focus {
            Focus::Repos => &mut self.repos.state,
            Focus::Tree => &mut self.tree.state,
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
            Focus::Tree => self.mark_preview_dirty(),
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
        let Some(rows_len) = self.zoom_doc().map(|doc| doc.rows_len()) else {
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

        let cursor = self.zoom_doc().map_or(0, |doc| doc.cursor());
        self.preview.scroll = new_top.min(u16::MAX as usize) as u16;
        if let Some(doc) = self.zoom_doc_mut() {
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

        // A zoomed JSONL preview: select the record under the pointer, and
        // unfold it on the second click, as a double-click does everywhere else.
        if self.zoom_doc().is_some()
            && let Some(area) = self.hits.preview
            && Hits::hit(area, col, row)
        {
            let Some(&line) = self.hits.preview_rows.get((row - area.y) as usize) else {
                return;
            };
            if let Some(doc) = self.zoom_doc_mut() {
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
                    .is_some_and(|r| r.reference.is_none() && r.expandable),
                Focus::Tree => self.tree.selected().is_some_and(|n| n.is_dir()),
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

/// The row showing screen line `line`, given the line each row starts at. A line
/// past the end of the body belongs to the last row, which is what the view
/// would be showing there anyway.
fn row_at_line(starts: &[usize], line: usize) -> usize {
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

/// A lakeFS object name reduced to something safe to create in the working
/// directory. The name is already a single path segment, so this is insurance
/// against a server that answers with something stranger — never a path.
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim();
    // `.`, `..` and the empty string all name something other than a new file.
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '.') {
        "download".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Create a file in `dir` to download into, never clobbering one already there:
/// `report.csv`, then `report (1).csv`, and so on.
///
/// Creation is exclusive, so the name is claimed by the same call that tests it
/// — two downloads racing for one name land on different files rather than
/// interleaving into a single corrupt one.
async fn create_download_file(dir: &Path, name: &str) -> Result<(tokio::fs::File, String)> {
    let name = safe_name(name);
    let path = Path::new(&name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.clone());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    for n in 0..1000 {
        let candidate = if n == 0 {
            name.clone()
        } else {
            format!("{stem} ({n}){ext}")
        };
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(&candidate))
            .await
        {
            Ok(file) => return Ok((file, candidate)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("creating {candidate}")));
            }
        }
    }
    bail!("{name}: too many files by that name here already")
}

/// Whether `path` names newline-delimited JSON. Decided by extension rather
/// than by sniffing: a `.jsonl` full of broken records is still JSONL, and
/// should say which records are broken instead of quietly rendering as text.
fn is_jsonl(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".jsonl") || lower.ends_with(".ndjson")
}

/// Decide whether the fetched bytes are text; fall back to a hex dump.
fn render_body(path: &str, bytes: &[u8], truncated: bool) -> PreviewBody {
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

    // One record per line, each folded up. Unlike whole-file JSON this survives
    // truncation — every record but the last still parses on its own.
    if is_jsonl(path) {
        return PreviewBody::Jsonl(crate::jsonl::parse(&text, truncated));
    }

    // Pretty-print JSON. A body truncated by `preview_bytes` won't parse, so
    // this quietly falls through to the plain-text path.
    let head = text.trim_start();
    if (head.starts_with('{') || head.starts_with('['))
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let mut lines = Vec::new();
        write_json(&value, 0, None, false, &mut lines);
        return PreviewBody::Json {
            lines,
            doc: crate::jsonl::JsonDoc::new(value),
        };
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

    // ── downloads ────────────────────────────────────────────────────────

    /// A scratch directory of its own per test, so the collision cases can't
    /// see each other's files.
    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lakeview-test-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_name_that_is_already_a_plain_filename_is_left_alone() {
        assert_eq!(safe_name("daily_rollup.json"), "daily_rollup.json");
        assert_eq!(safe_name("a b.tar.gz"), "a b.tar.gz");
    }

    #[test]
    fn a_name_that_could_escape_the_directory_cannot() {
        // lakeFS hands back a single segment, so these mean a server doing
        // something strange — none of them may name a path.
        assert_eq!(safe_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(safe_name(".."), "download");
        assert_eq!(safe_name("."), "download");
        assert_eq!(safe_name("   "), "download");
        assert_eq!(safe_name(""), "download");
    }

    #[tokio::test]
    async fn the_first_download_of_a_name_gets_the_name() {
        let dir = scratch("first");
        let (_file, name) = create_download_file(&dir, "report.csv").await.unwrap();
        assert_eq!(name, "report.csv");
        assert!(dir.join("report.csv").exists());
    }

    #[tokio::test]
    async fn a_second_download_never_overwrites_the_first() {
        let dir = scratch("collide");
        let names = [
            create_download_file(&dir, "report.csv").await.unwrap().1,
            create_download_file(&dir, "report.csv").await.unwrap().1,
            create_download_file(&dir, "report.csv").await.unwrap().1,
        ];
        assert_eq!(names, ["report.csv", "report (1).csv", "report (2).csv"]);
    }

    #[tokio::test]
    async fn an_extensionless_name_still_gets_a_suffix() {
        let dir = scratch("bare");
        assert_eq!(
            create_download_file(&dir, "LICENSE").await.unwrap().1,
            "LICENSE"
        );
        assert_eq!(
            create_download_file(&dir, "LICENSE").await.unwrap().1,
            "LICENSE (1)"
        );
    }

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

    fn test_app() -> App {
        let profile = Profile {
            // Nothing here is fetched; the client only has to exist.
            endpoint: "http://127.0.0.1:1".into(),
            access_key_id: "key".into(),
            secret_access_key: "secret".into(),
            default_repo: None,
            default_ref: None,
            verify_tls: true,
            timeout_secs: 1,
            description: None,
        };
        let client = Client::new(&profile, 500).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(Config::default(), "test".into(), profile, client, tx)
    }

    fn stat(path: &str, dir: bool) -> ObjectStats {
        ObjectStats {
            path: path.into(),
            path_type: if dir { "common_prefix" } else { "object" }.into(),
            size_bytes: (!dir).then_some(12),
        }
    }

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
        app.preview.body = Some(PreviewBody::Json {
            lines: Vec::new(),
            doc,
        });
        app.enter();
        assert_eq!(app.mode, Mode::Zoom);
        let open = app.zoom_doc().unwrap().rows_len();

        // Out to `"a"`, which folds; after that there is nowhere left to go, and
        // none of the presses leaves.
        for _ in 0..5 {
            app.back();
            assert_eq!(app.mode, Mode::Zoom, "`←` left the zoom");
        }
        let shut = app.zoom_doc().unwrap().rows_len();
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
}
