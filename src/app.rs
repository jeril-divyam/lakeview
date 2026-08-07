//! Application state and the update half of the loop.
//!
//! Navigation is a stack of panes (Miller columns): repositories → refs →
//! object prefixes. Opening an entry pushes a pane on the right; going back
//! pops it. All network work happens off-thread and reports back through
//! `Msg`; every request carries a monotonic id so stale replies are dropped.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{Config, Profile};
use crate::lakefs::{Client, Commit, NamedRef, ObjectStats, RefKind, Repository};

const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(150);
const STATUS_TTL: Duration = Duration::from_secs(4);

// ── messages from background tasks ───────────────────────────────────────

pub enum Msg {
    Repos(u64, Result<Vec<Repository>, String>),
    Refs(u64, Result<Vec<NamedRef>, String>),
    Objects(u64, Result<Vec<ObjectStats>, String>),
    Commits(u64, Result<Vec<Commit>, String>),
    Preview(u64, Result<PreviewPayload, String>),
}

pub struct PreviewPayload {
    pub stat: ObjectStats,
    pub bytes: Vec<u8>,
}

// ── panes ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Repos,
    Refs {
        repo: String,
    },
    Objects {
        repo: String,
        reference: String,
        prefix: String,
    },
}

pub enum Items {
    None,
    Repos(Vec<Repository>),
    Refs(Vec<NamedRef>),
    Objects(Vec<ObjectStats>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Repo,
    Branch,
    Tag,
    Dir,
    File,
}

/// A render-ready line, cached so filtering isn't redone every frame.
pub struct Row {
    pub label: String,
    pub meta: String,
    pub kind: RowKind,
    /// Highlights the repository's default branch.
    pub primary: bool,
    /// Index into the pane's unfiltered item list.
    pub index: usize,
}

pub enum Load {
    Loading,
    Ready,
    Failed(String),
}

pub struct Pane {
    pub source: Source,
    pub items: Items,
    pub rows: Vec<Row>,
    pub state: ListState,
    pub load: Load,
    pub filter: String,
    /// Id of the most recent request; replies with a different id are stale.
    pub req: u64,
}

impl Pane {
    fn new(source: Source, req: u64) -> Self {
        Self {
            source,
            items: Items::None,
            rows: Vec::new(),
            state: ListState::default(),
            load: Load::Loading,
            filter: String::new(),
            req,
        }
    }

    pub fn title(&self) -> String {
        match &self.source {
            Source::Repos => "Repositories".into(),
            Source::Refs { repo } => repo.clone(),
            Source::Objects {
                reference, prefix, ..
            } => {
                if prefix.is_empty() {
                    reference.clone()
                } else {
                    prefix
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or(prefix)
                        .into()
                }
            }
        }
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.state.selected().and_then(|i| self.rows.get(i))
    }

    pub fn selected_object(&self) -> Option<&ObjectStats> {
        let row = self.selected_row()?;
        match &self.items {
            Items::Objects(v) => v.get(row.index),
            _ => None,
        }
    }

    /// Recompute the visible rows, preserving the selected item where possible.
    fn rebuild(&mut self) {
        let previous = self.selected_row().map(|r| r.label.clone());
        let needle = self.filter.to_lowercase();
        let matches = |s: &str| needle.is_empty() || s.to_lowercase().contains(&needle);

        self.rows = match &self.items {
            Items::None => Vec::new(),
            Items::Repos(v) => v
                .iter()
                .enumerate()
                .filter(|(_, r)| matches(&r.id))
                .map(|(i, r)| Row {
                    label: r.id.clone(),
                    meta: r.default_branch.clone(),
                    kind: RowKind::Repo,
                    primary: false,
                    index: i,
                })
                .collect(),
            Items::Refs(v) => v
                .iter()
                .enumerate()
                .filter(|(_, r)| matches(&r.id))
                .map(|(i, r)| Row {
                    label: r.id.clone(),
                    meta: r.commit_id.chars().take(8).collect(),
                    kind: match r.kind {
                        RefKind::Branch => RowKind::Branch,
                        RefKind::Tag => RowKind::Tag,
                    },
                    primary: r.is_default,
                    index: i,
                })
                .collect(),
            Items::Objects(v) => v
                .iter()
                .enumerate()
                .filter(|(_, o)| matches(o.name()))
                .map(|(i, o)| Row {
                    label: o.name().to_string(),
                    meta: if o.is_dir() {
                        String::new()
                    } else {
                        crate::ui::human_size(o.size_bytes.unwrap_or(0))
                    },
                    kind: if o.is_dir() {
                        RowKind::Dir
                    } else {
                        RowKind::File
                    },
                    primary: false,
                    index: i,
                })
                .collect(),
        };

        let restored = previous.and_then(|label| self.rows.iter().position(|r| r.label == label));
        self.state.select(match restored {
            Some(i) => Some(i),
            None if self.rows.is_empty() => None,
            None => Some(0),
        });
    }

    fn set_items(&mut self, items: Items) {
        self.items = items;
        self.load = Load::Ready;
        self.rebuild();
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let current = self.state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, last as isize) as usize;
        self.state.select(Some(next));
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
    /// (pane index, inner list area) for each column currently on screen.
    pub columns: Vec<(usize, Rect)>,
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

    pub panes: Vec<Pane>,
    pub tab: Tab,
    pub mode: Mode,
    pub commits: CommitsView,
    pub preview: Preview,
    pub status: Option<Status>,
    pub should_quit: bool,

    next_req: u64,
    pub inflight: usize,
    pub tick: usize,
    /// Set when `open` is pressed on a column that is still loading; replayed
    /// once the data lands so fast drill-downs don't lose keystrokes.
    pub pending_open: bool,
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
            panes: Vec::new(),
            tab: Tab::Browse,
            mode: Mode::Normal,
            commits: CommitsView::default(),
            preview: Preview::default(),
            status: None,
            should_quit: false,
            next_req: 0,
            inflight: 0,
            tick: 0,
            pending_open: false,
            hits: Hits::default(),
            last_click: None,
        };
        app.reset_to_repos();
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

    pub fn focused(&self) -> &Pane {
        self.panes.last().expect("at least one pane")
    }

    fn focused_mut(&mut self) -> &mut Pane {
        self.panes.last_mut().expect("at least one pane")
    }

    /// The repo/ref the user is currently inside, if any.
    pub fn context(&self) -> (Option<&str>, Option<&str>) {
        let mut repo = None;
        let mut reference = None;
        for pane in &self.panes {
            match &pane.source {
                Source::Repos => {}
                Source::Refs { repo: r } => repo = Some(r.as_str()),
                Source::Objects {
                    repo: r,
                    reference: f,
                    ..
                } => {
                    repo = Some(r.as_str());
                    reference = Some(f.as_str());
                }
            }
        }
        (repo, reference)
    }

    pub fn breadcrumb(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some(pane) = self.panes.last() {
            match &pane.source {
                Source::Repos => {}
                Source::Refs { repo } => parts.push(repo.clone()),
                Source::Objects {
                    repo,
                    reference,
                    prefix,
                } => {
                    parts.push(repo.clone());
                    parts.push(reference.clone());
                    parts.extend(
                        prefix
                            .trim_end_matches('/')
                            .split('/')
                            .filter(|s| !s.is_empty())
                            .map(String::from),
                    );
                }
            }
        }
        parts
    }

    // ── loading ──────────────────────────────────────────────────────────

    pub fn reset_to_repos(&mut self) {
        let req = self.req_id();
        self.panes = vec![Pane::new(Source::Repos, req)];
        self.spawn_load(0);
    }

    /// Jump straight to `repo` (and optionally `reference`), building the
    /// intermediate columns so the user can still walk back up.
    pub fn open_path(&mut self, repo: &str, reference: Option<&str>) {
        // `spawn_load` assigns each pane its real request id below.
        let mut sources = vec![
            Source::Repos,
            Source::Refs {
                repo: repo.to_string(),
            },
        ];
        if let Some(reference) = reference {
            sources.push(Source::Objects {
                repo: repo.to_string(),
                reference: reference.to_string(),
                prefix: String::new(),
            });
        }

        self.panes = sources.into_iter().map(|s| Pane::new(s, 0)).collect();
        for idx in 0..self.panes.len() {
            self.spawn_load(idx);
        }
    }

    /// Kick off the fetch backing pane `idx`.
    fn spawn_load(&mut self, idx: usize) {
        let req = self.req_id();
        let (source, tx, client) = {
            let pane = &mut self.panes[idx];
            pane.req = req;
            pane.load = Load::Loading;
            (pane.source.clone(), self.tx.clone(), self.client.clone())
        };
        let show_tags = self.cfg.ui.show_tags;
        self.inflight += 1;

        tokio::spawn(async move {
            let msg = match source {
                Source::Repos => Msg::Repos(req, client.repositories().await.map_err(fmt_err)),
                Source::Refs { repo } => {
                    Msg::Refs(req, client.refs(&repo, show_tags).await.map_err(fmt_err))
                }
                Source::Objects {
                    repo,
                    reference,
                    prefix,
                } => Msg::Objects(
                    req,
                    client
                        .list_objects(&repo, &reference, &prefix)
                        .await
                        .map_err(fmt_err),
                ),
            };
            let _ = tx.send(msg);
        });
    }

    pub fn reload_focused(&mut self) {
        let idx = self.panes.len() - 1;
        self.spawn_load(idx);
        self.preview.key = None;
        self.mark_preview_dirty();
        self.set_status("reloading…", false);
    }

    pub fn load_commits(&mut self, force: bool) {
        let (repo, reference) = self.context();
        let (Some(repo), Some(reference)) = (repo.map(String::from), reference.map(String::from))
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

        let (repo, reference) = self.context();
        let (Some(repo), Some(reference)) = (repo.map(String::from), reference.map(String::from))
        else {
            self.clear_preview();
            return;
        };
        let Some(object) = self.focused().selected_object().cloned() else {
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

    // ── message handling ─────────────────────────────────────────────────

    pub fn on_msg(&mut self, msg: Msg) {
        self.inflight = self.inflight.saturating_sub(1);
        match msg {
            Msg::Repos(req, res) => self.apply_pane(req, res.map(Items::Repos)),
            Msg::Refs(req, res) => self.apply_pane(req, res.map(Items::Refs)),
            Msg::Objects(req, res) => self.apply_pane(req, res.map(Items::Objects)),

            Msg::Commits(req, res) => {
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

    fn apply_pane(&mut self, req: u64, res: Result<Items, String>) {
        let Some(idx) = self.panes.iter().position(|p| p.req == req) else {
            return; // pane was popped while the request was in flight
        };
        match res {
            Ok(items) => {
                self.panes[idx].set_items(items);
                // A repo with a single ref offers no choice, so that column
                // drops out and its replacement is already loading.
                if self.collapse_single_ref(idx) {
                    return;
                }
                // An ancestor column should highlight the entry we came through.
                self.sync_selection(idx);
                self.mark_preview_dirty();
                // Replay an `open` that was pressed while this column loaded.
                if self.pending_open && idx == self.panes.len() - 1 {
                    self.pending_open = false;
                    self.open();
                }
            }
            Err(e) => {
                self.pending_open = false;
                self.panes[idx].load = Load::Failed(e.clone());
                self.set_status(e, true);
            }
        }
    }

    /// A refs column holding exactly one ref is pure ceremony — there is
    /// nothing to pick — so drop it and go straight to the object listing.
    /// Returns true when the column was removed.
    fn collapse_single_ref(&mut self, idx: usize) -> bool {
        let Items::Refs(refs) = &self.panes[idx].items else {
            return false;
        };
        let Source::Refs { repo } = &self.panes[idx].source else {
            return false;
        };
        let [only] = refs.as_slice() else {
            return false;
        };
        let (repo, reference) = (repo.clone(), only.id.clone());

        let was_last = idx + 1 == self.panes.len();
        self.panes.remove(idx);

        if was_last {
            // Descend in its place. If the column had a child (a --repo/--ref
            // jump), that child already covers the listing.
            self.panes.insert(
                idx,
                Pane::new(
                    Source::Objects {
                        repo,
                        reference,
                        prefix: String::new(),
                    },
                    0,
                ),
            );
            self.spawn_load(idx);
            self.clear_preview();
        }
        true
    }

    /// Point pane `idx` at whichever row leads to pane `idx + 1`.
    fn sync_selection(&mut self, idx: usize) {
        let Some(child) = self.panes.get(idx + 1) else {
            return;
        };
        // What the parent lists decides which part of the child identifies it.
        let wanted = match (&self.panes[idx].source, &child.source) {
            (Source::Repos, Source::Refs { repo }) => repo.clone(),
            // No refs column in between: this repo had a single ref.
            (Source::Repos, Source::Objects { repo, .. }) => repo.clone(),
            (Source::Refs { .. }, Source::Objects { reference, .. }) => reference.clone(),
            (Source::Objects { .. }, Source::Objects { prefix, .. }) => prefix
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or(prefix)
                .to_string(),
            _ => return,
        };
        let pane = &mut self.panes[idx];
        if let Some(pos) = pane.rows.iter().position(|r| r.label == wanted) {
            pane.state.select(Some(pos));
        }
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.poll_preview();
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
                if self.commits.commits.is_empty() {
                    return;
                }
                let last = self.commits.commits.len() - 1;
                let current = self.commits.state.selected().unwrap_or(0) as isize;
                let next = (current + delta).clamp(0, last as isize) as usize;
                self.commits.state.select(Some(next));
            }
            _ if self.mode == Mode::Zoom => {
                self.preview.scroll = self
                    .preview
                    .scroll
                    .saturating_add_signed(delta.clamp(-1000, 1000) as i16);
            }
            _ => {
                self.focused_mut().move_by(delta);
                self.mark_preview_dirty();
            }
        }
    }

    pub fn select_edge(&mut self, first: bool) {
        match self.tab {
            Tab::Commits => {
                let len = self.commits.commits.len();
                if len > 0 {
                    self.commits
                        .state
                        .select(Some(if first { 0 } else { len - 1 }));
                }
            }
            _ => {
                let pane = self.focused_mut();
                if !pane.rows.is_empty() {
                    let idx = if first { 0 } else { pane.rows.len() - 1 };
                    pane.state.select(Some(idx));
                }
                self.mark_preview_dirty();
            }
        }
    }

    /// Descend into the selected entry, opening a new pane on the right.
    pub fn open(&mut self) {
        // Nothing to open yet — remember the intent and replay it on arrival.
        if matches!(self.focused().load, Load::Loading) {
            self.pending_open = true;
            return;
        }
        let Some(row) = self.focused().selected_row() else {
            return;
        };
        let index = row.index;

        let next = match (&self.focused().source, &self.focused().items) {
            (Source::Repos, Items::Repos(v)) => {
                v.get(index).map(|r| Source::Refs { repo: r.id.clone() })
            }
            (Source::Refs { repo }, Items::Refs(v)) => v.get(index).map(|r| Source::Objects {
                repo: repo.clone(),
                reference: r.id.clone(),
                prefix: String::new(),
            }),
            (
                Source::Objects {
                    repo, reference, ..
                },
                Items::Objects(v),
            ) => {
                match v.get(index) {
                    Some(o) if o.is_dir() => Some(Source::Objects {
                        repo: repo.clone(),
                        reference: reference.clone(),
                        prefix: o.path.clone(),
                    }),
                    // A file has nothing to descend into — zoom the preview.
                    Some(_) => {
                        self.mode = Mode::Zoom;
                        self.preview.scroll = 0;
                        None
                    }
                    None => None,
                }
            }
            _ => None,
        };

        if let Some(source) = next {
            let req = self.req_id();
            self.panes.push(Pane::new(source, req));
            let idx = self.panes.len() - 1;
            self.spawn_load(idx);
            self.clear_preview();
        }
    }

    /// Pop the rightmost pane.
    pub fn back(&mut self) {
        if self.mode == Mode::Zoom {
            self.mode = Mode::Normal;
            return;
        }
        if self.panes.len() > 1 {
            self.panes.pop();
            self.clear_preview();
            self.mark_preview_dirty();
        }
    }

    /// Absolute path of the current selection, for the `y` (copy) action.
    pub fn selection_uri(&self) -> Option<String> {
        let pane = self.focused();
        let row = pane.selected_row()?;
        match (&pane.source, &pane.items) {
            (Source::Repos, Items::Repos(v)) => {
                v.get(row.index).map(|r| format!("lakefs://{}", r.id))
            }
            (Source::Refs { repo }, Items::Refs(v)) => v
                .get(row.index)
                .map(|r| format!("lakefs://{}/{}", repo, r.id)),
            (
                Source::Objects {
                    repo, reference, ..
                },
                Items::Objects(v),
            ) => v
                .get(row.index)
                .map(|o| format!("lakefs://{}/{}/{}", repo, reference, o.path)),
            _ => None,
        }
    }

    // ── tabs ─────────────────────────────────────────────────────────────

    pub fn select_tab(&mut self, tab: Tab) {
        self.tab = tab;
        if tab == Tab::Commits {
            let (repo, reference) = self.context();
            if repo.is_none() || reference.is_none() {
                self.set_status("open a repository and ref in Browse first", false);
            }
            self.load_commits(false);
        }
    }

    // ── mouse ────────────────────────────────────────────────────────────

    /// Map a screen cell inside a column to (pane index, row index).
    fn column_at(&self, col: u16, row: u16) -> Option<(usize, usize)> {
        let (idx, area) = self
            .hits
            .columns
            .iter()
            .find(|(_, area)| Hits::hit(*area, col, row))?;
        let pane = self.panes.get(*idx)?;
        let line = pane.state.offset() + (row - area.y) as usize;
        (line < pane.rows.len()).then_some((*idx, line))
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

        let Some((idx, area)) = self
            .hits
            .columns
            .iter()
            .find(|(_, area)| Hits::hit(*area, col, row))
            .map(|(i, a)| (*i, *a))
        else {
            return;
        };

        if idx == self.panes.len() - 1 {
            // The focused column tracks the wheel like j/k, so the preview
            // follows along.
            self.move_selection(delta);
        } else {
            // An ancestor just peeks: scroll the view, leave the selection and
            // the pane stack alone. Stop once the last row is on screen, so a
            // list shorter than its viewport doesn't scroll at all.
            let pane = &mut self.panes[idx];
            let max = pane.rows.len().saturating_sub(area.height as usize);
            let offset = pane.state.offset_mut();
            *offset = if down {
                (*offset + WHEEL_LINES).min(max)
            } else {
                offset.saturating_sub(WHEEL_LINES)
            };
        }
    }

    /// Left click: select, or open when it's a double-click. Clicking into an
    /// ancestor column closes the columns to its right, like a file browser.
    pub fn mouse_click(&mut self, col: u16, row: u16) {
        self.pending_open = false;

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

        let Some((idx, line)) = self.column_at(col, row) else {
            return;
        };

        // Focus the clicked column by dropping everything to its right.
        if idx + 1 < self.panes.len() {
            self.panes.truncate(idx + 1);
            self.clear_preview();
        }
        self.panes[idx].state.select(Some(line));
        self.mark_preview_dirty();

        if double {
            self.open();
        }
    }

    /// Right click mirrors `h`: close the rightmost column.
    pub fn mouse_back(&mut self) {
        self.pending_open = false;
        self.back();
    }

    // ── filter ───────────────────────────────────────────────────────────

    pub fn filter_push(&mut self, c: char) {
        self.focused_mut().filter.push(c);
        self.focused_mut().rebuild();
        self.mark_preview_dirty();
    }

    pub fn filter_pop(&mut self) {
        self.focused_mut().filter.pop();
        self.focused_mut().rebuild();
        self.mark_preview_dirty();
    }

    pub fn filter_clear(&mut self) {
        self.focused_mut().filter.clear();
        self.focused_mut().rebuild();
        self.mark_preview_dirty();
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
                self.clear_preview();
                self.reset_to_repos();
                self.set_status(format!("switched to profile `{name}`"), false);
            }
            Err(e) => self.set_status(format!("{name}: {e}"), true),
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
