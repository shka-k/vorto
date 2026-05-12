mod action;
mod app;
mod config;
mod editor;
mod fuzzy;
mod highlight;
mod keymap;
mod languages;
mod mode;
mod search;
mod theme;
mod ui;

use std::io::{self, Stdout, Write};

use anyhow::Result;
use crossterm::event::{self, Event};
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

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut keymap = crate::keymap::Keymap::vim_default();
    let cfg_path = config::default_path();
    let cfg = config::load_or_default(cfg_path.as_deref())?;
    config::apply(&cfg, &mut keymap)?;

    let languages = languages::resolve(cfg.languages.clone());
    let extension_index = languages::build_extension_index(&languages);
    let loader = highlight::Loader::new(config::grammar_dir(&cfg), config::query_dir(&cfg));

    let mut app = App::new(keymap, loader, languages, extension_index);
    app.cursor_shapes = config::resolve_cursor_shapes(&cfg.cursor)?;
    if let Some(p) = path {
        app.open_path(std::path::Path::new(&p))?;
    }

    let result = run(&mut terminal, &mut app);

    disable_raw_mode()?;
    // `\x1b[0 q` = DECSCUSR Ps=0 → restore the user's configured shape.
    let _ = io::stdout().write_all(b"\x1b[0 q");
    let _ = io::stdout().flush();
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    let mut last_shape: Option<CursorShape> = None;
    while !app.should_quit {
        app.buffer.refresh_highlights();
        terminal.draw(|f| ui::draw(f, app))?;
        let shape = app.cursor_shapes.for_mode(app.mode);
        if last_shape != Some(shape) {
            let mut out = io::stdout();
            out.write_all(shape.ansi())?;
            out.flush()?;
            last_shape = Some(shape);
        }
        if let Event::Key(key) = event::read()? {
            app.handle_key(key)?;
        }
    }
    Ok(())
}
