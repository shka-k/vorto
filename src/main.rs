mod action;
mod app;
mod config;
mod editor;
mod fuzzy;
mod keymap;
mod mode;
mod search;
mod ui;

use std::io;

use anyhow::Result;
use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::App;

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

    let mut app = App::with_keymap(keymap);
    if let Some(p) = path {
        app.open_path(std::path::Path::new(&p))?;
    }

    let result = run(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|f| ui::draw(f, app))?;
        if let Event::Key(key) = event::read()? {
            app.handle_key(key)?;
        }
    }
    Ok(())
}
