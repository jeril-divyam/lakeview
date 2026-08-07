//! lakeview — a terminal browser for lakeFS.

mod app;
mod config;
mod jsonl;
mod lakefs;
mod theme;
mod ui;

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use ratatui::crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;

use app::{App, Mode, Msg, Tab};
use config::Config;
use lakefs::Client;

#[derive(Parser)]
#[command(
    name = "lakeview",
    version,
    about = "A terminal browser for lakeFS",
    long_about = "Browse lakeFS repositories, refs and objects from the terminal.\n\
                  Profiles are read from ~/.config/lakeview.toml."
)]
struct Cli {
    /// Profile to use (defaults to `default_profile` in the config).
    #[arg(short, long, global = true)]
    profile: Option<String>,

    /// Path to the config file.
    #[arg(short, long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Open this repository on start-up.
    #[arg(long, value_name = "REPO")]
    repo: Option<String>,

    /// Open this ref on start-up (requires --repo).
    #[arg(long = "ref", value_name = "REF")]
    reference: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Write a starter config file.
    Init {
        /// Overwrite an existing config file.
        #[arg(long)]
        force: bool,
    },
    /// List the configured profiles.
    Profiles,
    /// Check that a profile can reach its lakeFS server.
    Check,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = match &cli.config {
        Some(p) => p.clone(),
        None => Config::default_path()?,
    };

    match cli.command {
        Some(Command::Init { force }) => return cmd_init(&path, force),
        Some(Command::Profiles) => return cmd_profiles(&path),
        Some(Command::Check) => return cmd_check(&path, cli.profile.as_deref()),
        None => {}
    }

    if !path.exists() {
        bail!(
            "no config at {} — run `lakeview init` to create one",
            path.display()
        );
    }
    let cfg = Config::load(&path)?;
    let (name, profile) = cfg.select(cli.profile.as_deref())?;
    let client = Client::new(&profile, cfg.ui.page_size)?;

    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    runtime.block_on(async {
        // Fail loudly here rather than inside the alternate screen.
        client
            .verify()
            .await
            .with_context(|| format!("profile `{name}` ({})", profile.endpoint))?;

        let repo = cli.repo.clone().or_else(|| profile.default_repo.clone());
        let reference = cli
            .reference
            .clone()
            .or_else(|| profile.default_ref.clone());

        run_tui(cfg, name, profile, client, repo, reference).await
    })
}

async fn run_tui(
    cfg: Config,
    profile_name: String,
    profile: config::Profile,
    client: Client,
    repo: Option<String>,
    reference: Option<String>,
) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
    let mut app = App::new(cfg, profile_name, profile, client, tx);
    if let Some(repo) = repo {
        app.open_path(&repo, reference.as_deref());
    }

    let mouse = app.cfg.ui.mouse;
    let mut terminal = ratatui::try_init().context("entering the alternate screen")?;
    if mouse {
        execute!(std::io::stdout(), EnableMouseCapture).context("enabling mouse capture")?;
        // ratatui's panic hook restores the screen but knows nothing about
        // mouse capture; chain onto it so a panic can't leave the terminal
        // spewing escape sequences.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = execute!(std::io::stdout(), DisableMouseCapture);
            previous(info);
        }));
    }

    let result = event_loop(&mut terminal, &mut app, &mut rx).await;

    if mouse {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Msg>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => on_key(app, key),
                    Some(Ok(Event::Mouse(mouse))) => on_mouse(app, mouse),
                    Some(Ok(Event::Resize(_, _))) => {}
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e).context("reading terminal input"),
                    None => return Ok(()),
                }
            }
            Some(msg) = rx.recv() => {
                app.on_msg(msg);
                // Drain anything else already queued before redrawing.
                while let Ok(extra) = rx.try_recv() {
                    app.on_msg(extra);
                }
            }
            _ = ticker.tick() => app.on_tick(),
        }
    }
}

fn on_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && matches!(key.code, KeyCode::Char('c')) {
        app.should_quit = true;
        return;
    }

    match app.mode.clone() {
        Mode::Filter => on_key_filter(app, key),
        Mode::Profiles(selected) => on_key_profiles(app, key, selected),
        Mode::Normal | Mode::Zoom => on_key_normal(app, key, ctrl),
    }
}

fn on_mouse(app: &mut App, mouse: MouseEvent) {
    let (col, row) = (mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::ScrollDown => app.mouse_scroll(col, row, true),
        MouseEventKind::ScrollUp => app.mouse_scroll(col, row, false),
        // Overlays are keyboard-driven; a click elsewhere just dismisses them.
        MouseEventKind::Down(MouseButton::Left) => match app.mode {
            Mode::Profiles(_) | Mode::Filter => app.mode = Mode::Normal,
            Mode::Zoom | Mode::Normal => app.mouse_click(col, row),
        },
        MouseEventKind::Down(MouseButton::Right) => app.mouse_back(),
        _ => {}
    }
}

fn on_key_filter(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.filter_clear();
            app.mode = Mode::Normal;
        }
        KeyCode::Enter => app.mode = Mode::Normal,
        KeyCode::Backspace => app.filter_pop(),
        KeyCode::Down => app.move_selection(1),
        KeyCode::Up => app.move_selection(-1),
        KeyCode::Char(c) => app.filter_push(c),
        _ => {}
    }
}

fn on_key_profiles(app: &mut App, key: KeyEvent, selected: usize) {
    let names = app.profile_names();
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('p') => app.mode = Mode::Normal,
        KeyCode::Char('j') | KeyCode::Down => {
            let next = (selected + 1).min(names.len().saturating_sub(1));
            app.mode = Mode::Profiles(next);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.mode = Mode::Profiles(selected.saturating_sub(1));
        }
        KeyCode::Enter => {
            if let Some(name) = names.get(selected).cloned() {
                app.mode = Mode::Normal;
                if name != app.profile_name {
                    app.switch_profile(&name);
                }
            }
        }
        _ => {}
    }
}

fn on_key_normal(app: &mut App, key: KeyEvent, ctrl: bool) {
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Esc => {
            if app.mode == Mode::Zoom {
                app.mode = Mode::Normal;
            } else if !app.filter().is_empty() {
                app.filter_clear();
            }
        }

        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('d') if ctrl => app.move_selection(10),
        KeyCode::Char('u') if ctrl => app.move_selection(-10),
        KeyCode::PageDown => app.move_selection(10),
        KeyCode::PageUp => app.move_selection(-10),
        KeyCode::Char('g') | KeyCode::Home => app.select_edge(true),
        KeyCode::Char('G') | KeyCode::End => app.select_edge(false),

        KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
            if app.tab == Tab::Browse {
                app.open();
            }
        }
        KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
            if app.tab == Tab::Browse {
                app.back();
            }
        }
        // Expand or collapse in place, without moving focus. Zoomed, the only
        // thing with anything to fold is a JSONL record; `toggle` knows.
        KeyCode::Char(' ') => {
            if app.tab == Tab::Browse {
                app.toggle();
            }
        }

        KeyCode::Char('/') => {
            if app.tab == Tab::Browse {
                app.mode = Mode::Filter;
            }
        }
        KeyCode::Char('r') => match app.tab {
            Tab::Commits => app.load_commits(true),
            _ => app.reload_focused(),
        },
        KeyCode::Char('y') => copy_selection(app),
        KeyCode::Char('p') => {
            let current = app
                .profile_names()
                .iter()
                .position(|n| *n == app.profile_name)
                .unwrap_or(0);
            app.mode = Mode::Profiles(current);
        }

        KeyCode::Char('?') => app.tab = Tab::Help,
        KeyCode::Char('1') => app.select_tab(Tab::Browse),
        KeyCode::Char('2') => app.select_tab(Tab::Commits),
        KeyCode::Char('3') => app.select_tab(Tab::Help),
        KeyCode::Tab => {
            let next = match app.tab {
                Tab::Browse => Tab::Commits,
                Tab::Commits => Tab::Help,
                Tab::Help => Tab::Browse,
            };
            app.select_tab(next);
        }
        KeyCode::BackTab => {
            let prev = match app.tab {
                Tab::Browse => Tab::Help,
                Tab::Commits => Tab::Browse,
                Tab::Help => Tab::Commits,
            };
            app.select_tab(prev);
        }
        _ => {}
    }
}

/// Copy via OSC 52 so it works over SSH without a clipboard daemon.
fn copy_selection(app: &mut App) {
    let Some(uri) = app.selection_uri() else {
        return;
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(uri.as_bytes());
    let mut stdout = std::io::stdout();
    let ok = write!(stdout, "\x1b]52;c;{encoded}\x07").and_then(|_| stdout.flush());
    match ok {
        Ok(()) => app.set_status(format!("copied {uri}"), false),
        Err(e) => app.set_status(format!("copy failed: {e}"), true),
    }
}

// ── subcommands ──────────────────────────────────────────────────────────

fn cmd_init(path: &std::path::Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists — pass --force to overwrite",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    // Seed from ~/.lakectl.yaml when it's there, so the file works as written.
    let contents = match config::read_lakectl() {
        Some((endpoint, key, secret)) => {
            println!("found ~/.lakectl.yaml — seeding the `local` profile from it");
            config::TEMPLATE
                .replace("http://localhost:8000", &endpoint)
                .replace("AKIAIOSFOLQUICKSTART", &key)
                .replace("${LAKEFS_SECRET_ACCESS_KEY}", &secret)
        }
        None => config::TEMPLATE.to_string(),
    };

    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }

    println!("wrote {}", path.display());
    println!("edit it, then run `lakeview` to connect.");
    Ok(())
}

fn cmd_profiles(path: &std::path::Path) -> Result<()> {
    let cfg = Config::load(path)?;
    if cfg.profiles.is_empty() {
        println!("no profiles in {}", path.display());
        return Ok(());
    }
    let default = cfg.default_profile.clone().unwrap_or_default();
    let width = cfg.profiles.keys().map(String::len).max().unwrap_or(0);
    for (name, profile) in &cfg.profiles {
        let marker = if *name == default { "*" } else { " " };
        let note = profile
            .description
            .as_deref()
            .map(|d| format!("  ({d})"))
            .unwrap_or_default();
        println!(
            "{marker} {name:width$}  {}{note}",
            profile.endpoint.trim_end_matches('/')
        );
    }
    Ok(())
}

fn cmd_check(path: &std::path::Path, requested: Option<&str>) -> Result<()> {
    let cfg = Config::load(path)?;
    let (name, profile) = cfg.select(requested)?;
    let client = Client::new(&profile, cfg.ui.page_size)?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        client.verify().await?;
        let repos = client.repositories().await?;
        println!(
            "ok — profile `{name}` reached {} ({} repositories)",
            profile.endpoint.trim_end_matches('/'),
            repos.len()
        );
        Ok(())
    })
}
