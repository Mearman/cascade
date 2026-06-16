//! Terminal UI over the engine: a status panel and a file browser with
//! pin/unpin, over the same engine the daemon and the file verbs use.
//!
//! `cascade tui` opens an alternate-screen, raw-mode interface. The top pane
//! shows daemon state and the configured backends; the bottom pane browses the
//! VFS (descend into directories, ascend back out) and can pin or unpin the
//! selected entry through the same `CacheManager` the `pin`/`unpin` commands
//! use. No mount is involved — the TUI drives the engine directly, like
//! `ls`/`cat`/`mkdir`.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use cascade_engine::db::StateDb;
use cascade_engine::engine::NativeEngine;
use cascade_engine::types::DirEntry;
use crossterm::ExecutableCommand;
use crossterm::cursor::Hide;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use super::cache::make_manager;
use super::files::{build_engine, list_dir};
use super::{CliContext, is_process_alive};

type Term = io::Stdout;

/// Run the TUI to completion, always restoring the terminal on exit.
pub async fn run(ctx: &CliContext) -> Result<()> {
    let engine = build_engine(ctx).context("building engine for the TUI")?;
    let db = Arc::new(StateDb::open(&ctx.db_path).context("opening state database for the TUI")?);

    let mut terminal = setup_terminal()?;
    let mut app = App {
        current_path: "/".to_owned(),
        entries: Vec::new(),
        selected: 0,
        message: String::new(),
        running: true,
    };
    app.refresh(&engine).await;

    let result = event_loop(&mut terminal, ctx, &engine, &db, &mut app).await;
    restore_terminal()?;
    result
}

/// Install the alternate screen and raw mode, returning the ratatui terminal.
/// The terminal is always restored on exit through [`restore_terminal`], even
/// on error, so a panic or a `?` failure never leaves the operator with a
/// broken terminal.
fn setup_terminal() -> Result<ratatui::Terminal<CrosstermBackend<Term>>> {
    let mut stdout = io::stdout();
    stdout
        .execute(EnterAlternateScreen)
        .context("entering the alternate screen")?;
    enable_raw_mode().context("enabling raw mode for the TUI")?;
    let _ = stdout.execute(Hide);
    let backend = CrosstermBackend::new(stdout);
    ratatui::Terminal::new(backend).context("building the ratatui terminal")
}

/// Tear down the alternate screen and raw mode.
fn restore_terminal() -> Result<()> {
    use crossterm::cursor::Show;
    use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};

    let mut stdout = io::stdout();
    let _ = stdout.execute(Show);
    let _ = stdout.execute(LeaveAlternateScreen);
    disable_raw_mode().context("disabling raw mode")?;
    Ok(())
}

/// The TUI's mutable state: the directory being browsed, its entries, the
/// selected row, a transient message line, and the run flag.
struct App {
    current_path: String,
    entries: Vec<DirEntry>,
    selected: usize,
    message: String,
    running: bool,
}

impl App {
    /// Reload the current directory's entries, clamping the selection.
    async fn refresh(&mut self, engine: &NativeEngine) {
        match list_dir(engine, &self.current_path).await {
            Ok(entries) => {
                self.entries = entries;
                if self.selected >= self.entries.len() {
                    self.selected = self.entries.len().saturating_sub(1);
                }
            }
            Err(e) => {
                self.message = format!("could not list {path}: {e}", path = self.current_path);
            }
        }
    }

    /// The VFS path of the currently selected entry, or none when the list is
    /// empty.
    fn selected_path(&self) -> Option<String> {
        let entry = self.entries.get(self.selected)?;
        Some(join_vfs_path(&self.current_path, &entry.name))
    }
}

/// Drive the event loop: render, wait briefly for input, handle a key, repeat
/// until the operator quits. Engine operations (listing, pinning) are awaited
/// inline; the blocking event poll uses a short timeout so the screen still
/// redraws promptly on resize or refresh.
async fn event_loop(
    terminal: &mut ratatui::Terminal<CrosstermBackend<Term>>,
    ctx: &CliContext,
    engine: &NativeEngine,
    db: &Arc<StateDb>,
    app: &mut App,
) -> Result<()> {
    while app.running {
        terminal.draw(|frame| draw(frame, ctx, db, app))?;
        if !event::poll(Duration::from_millis(300)).context("polling terminal events")? {
            continue;
        }
        let Event::Key(key) = event::read().context("reading a terminal event")? else {
            continue;
        };
        handle_key(ctx, engine, db, app, key).await?;
    }
    Ok(())
}

/// Act on a single key press.
async fn handle_key(
    ctx: &CliContext,
    engine: &NativeEngine,
    db: &Arc<StateDb>,
    app: &mut App,
    key: KeyEvent,
) -> Result<()> {
    let count = app.entries.len();
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => app.running = false,
        KeyCode::Char('q') | KeyCode::Esc => app.running = false,
        KeyCode::Char('j') | KeyCode::Down if count > 0 => {
            app.selected = usize::min(app.selected + 1, count - 1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.selected = app.selected.saturating_sub(1);
        }
        KeyCode::Char('g') => app.selected = 0,
        KeyCode::Char('G') if count > 0 => app.selected = count - 1,
        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
            if let Some(entry) = app.entries.get(app.selected)
                && entry.is_dir
            {
                app.current_path = join_vfs_path(&app.current_path, &entry.name);
                app.selected = 0;
                app.refresh(engine).await;
            }
        }
        KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
            if app.current_path != "/" {
                app.current_path = parent_vfs_path(&app.current_path);
                app.selected = 0;
                app.refresh(engine).await;
            }
        }
        KeyCode::Char('p') => {
            if let Some(path) = app.selected_path() {
                let manager = make_manager(db.clone());
                match manager.pin(&path, true).await {
                    Ok(()) => app.message = format!("pinned {path}"),
                    Err(e) => app.message = format!("could not pin {path}: {e}"),
                }
            }
        }
        KeyCode::Char('P') => {
            if let Some(path) = app.selected_path() {
                let manager = make_manager(db.clone());
                match manager.unpin(&path).await {
                    Ok(true) => app.message = format!("unpinned {path}"),
                    Ok(false) => app.message = format!("not pinned: {path}"),
                    Err(e) => app.message = format!("could not unpin {path}: {e}"),
                }
            }
        }
        KeyCode::Char('r') => {
            app.message.clear();
            app.refresh(engine).await;
        }
        _ => {}
    }
    // Touch `ctx` so the signature stays honest even before future status panes
    // use it (daemon PID, p2p identity). The status read itself goes through the
    // shared state DB.
    let _ = ctx.pid_path;
    Ok(())
}

/// Render the two panes plus a footer.
fn draw(frame: &mut Frame, ctx: &CliContext, db: &Arc<StateDb>, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(status_height(db)),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(area);

    // The layout has exactly three constraints, so it always splits into three
    // rectangles; destructure with a slice pattern to avoid index arithmetic.
    let [status_area, files_area, footer_area] = chunks.as_ref() else {
        return;
    };
    draw_status(frame, *status_area, ctx, db);
    draw_files(frame, *files_area, app);
    draw_footer(frame, *footer_area, app);
}

/// Height of the status pane, sized to fit the backend list (clamped).
fn status_height(db: &Arc<StateDb>) -> u16 {
    let backends = db.list_backends().map_or(0, |b| b.len());
    // Header + daemon line + one line per backend, with a floor and ceiling.
    3u16.saturating_add(u16::try_from(backends).unwrap_or(u16::MAX))
        .min(12)
}

/// The top pane: daemon running state and the configured backends.
fn draw_status(frame: &mut Frame, area: Rect, ctx: &CliContext, db: &Arc<StateDb>) {
    let mut lines = Vec::new();

    let running = ctx
        .pid_path
        .to_str()
        .and_then(|_| std::fs::read_to_string(&ctx.pid_path).ok())
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .is_some_and(is_process_alive);
    lines.push(format!(
        "Cascade  daemon: {}  db: {}",
        if running { "running" } else { "stopped" },
        ctx.db_path.display()
    ));

    match db.list_backends() {
        Ok(backends) if backends.is_empty() => lines.push("No backends configured.".to_owned()),
        Ok(backends) => {
            lines.push(format!("Backends ({}):", backends.len()));
            for b in &backends {
                let mount = b.mount_path.clone().unwrap_or_else(|| "/".to_owned());
                lines.push(format!(
                    "  {} ({}) — mounted at {}",
                    b.display_name, b.backend_type, mount
                ));
            }
        }
        Err(e) => lines.push(format!("could not read backends: {e}")),
    }

    let block = Block::default().borders(Borders::ALL).title("Status");
    let text = lines.into_iter().map(ratatui::text::Line::from);
    frame.render_widget(Paragraph::new(text.collect::<Vec<_>>()).block(block), area);
}

/// The bottom pane: a selectable list of the current directory's entries.
fn draw_files(frame: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|entry| {
            let kind = if entry.is_dir { "dir " } else { "file" };
            ListItem::new(format!("{kind}  {}", entry.name))
        })
        .collect();

    let title = format!("Files — {}", app.current_path);
    let block = Block::default().borders(Borders::ALL).title(title);

    let mut state = ListState::default();
    if !app.entries.is_empty() {
        state.select(Some(app.selected));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut state);
}

/// The footer: keybindings and the transient message line.
fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let help = "q quit · j/k move · enter descend · h ascend · p pin · P unpin · r refresh";
    let mut lines = vec![ratatui::text::Line::from(help)];
    if !app.message.is_empty() {
        lines.push(ratatui::text::Line::from(app.message.as_str()));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Join a VFS directory path with a child name, keeping the "/" root form.
fn join_vfs_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The parent of a VFS path, or "/" at the root.
fn parent_vfs_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    match trimmed.rsplit_once('/') {
        Some(("", _)) | None => "/".to_owned(),
        Some((parent, _)) => format!("/{parent}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn join_keeps_root_form() {
        assert_eq!(join_vfs_path("/", "local"), "/local");
        assert_eq!(join_vfs_path("/local", "docs"), "/local/docs");
    }

    #[test]
    fn parent_walks_to_root() {
        assert_eq!(parent_vfs_path("/local/docs"), "/local");
        assert_eq!(parent_vfs_path("/local"), "/");
        assert_eq!(parent_vfs_path("/"), "/");
    }
}
