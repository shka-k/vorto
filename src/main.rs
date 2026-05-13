mod action;
mod app;
mod config;
mod editor;
mod effect;
mod event;
mod fuzzy;
mod highlight;
mod lsp;
mod mode;
mod preview;
mod prompt;
mod search;
mod theme;
mod ui;

use std::io::{self, Stdout, Write};
use std::sync::mpsc;
use std::thread;

use anyhow::Result;
use crossterm::event::{self as crossterm_event, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::App;
use crate::config::CursorShape;

fn main() -> Result<()> {
    let path = std::env::args().nth(1);
    // Anchor for LSP workspace root discovery — captured once here so the
    // value can't shift mid-session if anything changes the process's
    // cwd. Every later `:e` resolves against the same directory.
    let startup_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let cfg = config::Config::load(config::default_path().as_deref())?;
    let loader = highlight::Loader::new(cfg.grammar_dir.clone(), cfg.query_dir.clone());

    // Unified event channel. Terminal input runs on a dedicated thread
    // that pushes `Event::Term`; LSP reader threads push `Event::Lsp`.
    let (event_tx, event_rx) = mpsc::channel::<event::AppEvent>();
    let input_tx = event_tx.clone();
    thread::spawn(move || {
        loop {
            match crossterm_event::read() {
                Ok(ev) => {
                    if input_tx.send(event::AppEvent::Term(ev)).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    let mut app = App::new(cfg, loader, event_tx, startup_cwd);
    if let Some(p) = path {
        app.open_path(std::path::Path::new(&p))?;
    }

    let result = run(&mut terminal, &mut app, &event_rx);

    disable_raw_mode()?;
    // `\x1b[0 q` = DECSCUSR Ps=0 → restore the user's configured shape.
    let _ = io::stdout().write_all(b"\x1b[0 q");
    let _ = io::stdout().flush();
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    event_rx: &mpsc::Receiver<event::AppEvent>,
) -> Result<()> {
    let mut last_shape: Option<CursorShape> = None;
    while !app.should_quit {
        app.buffer.refresh_highlights();
        terminal.draw(|f| ui::draw(f, app))?;
        let shape = app.config.cursor_shapes.for_mode(app.mode);
        if last_shape != Some(shape) {
            let mut out = io::stdout();
            out.write_all(cursor_ansi(shape))?;
            out.flush()?;
            last_shape = Some(shape);
        }
        // Block on the next event. Both terminal input and LSP reader
        // threads feed this channel, so we wake on whichever comes first
        // and only redraw once after we drain the burst.
        let first = match event_rx.recv() {
            Ok(ev) => ev,
            Err(_) => return Ok(()),
        };
        dispatch(app, first)?;
        // Drain any events that piled up while we were blocked so we
        // don't redraw between a Term+Lsp pair (e.g. didChange burst).
        while let Ok(ev) = event_rx.try_recv() {
            dispatch(app, ev)?;
        }
        app.sync_buffer_if_dirty();
    }
    Ok(())
}

fn dispatch(app: &mut App, ev: event::AppEvent) -> Result<()> {
    match ev {
        event::AppEvent::Term(Event::Key(key)) => app.handle_key(key)?,
        event::AppEvent::Term(_) => {}
        event::AppEvent::Lsp(lsp_ev) => app.handle_lsp_event(lsp_ev),
        event::AppEvent::HighlighterReady { generation, result } => {
            app.handle_highlighter_ready(generation, result);
        }
        event::AppEvent::LspReady {
            generation,
            lang,
            path,
            result,
        } => {
            app.handle_lsp_ready(generation, lang, path, result);
        }
        event::AppEvent::PreviewReady(entry) => app.handle_preview_ready(entry),
    }
    Ok(())
}

/// DECSCUSR escape sequence — `CSI Ps SP q`, where Ps picks the shape.
/// Written directly to stdout from the main loop so the terminal
/// switches shape as the user changes mode.
fn cursor_ansi(shape: CursorShape) -> &'static [u8] {
    match shape {
        CursorShape::Block => b"\x1b[2 q",
        CursorShape::Bar => b"\x1b[6 q",
        CursorShape::Underbar => b"\x1b[4 q",
    }
}
