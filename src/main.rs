mod action;
mod app;
mod buffer_ref;
mod config;
mod copilot;
mod editor;
mod effect;
mod event;
mod finder;
mod format;
mod grammar;
mod log;
mod lsp;
mod mode;
mod prompt;
mod syntax;
mod text_width;
mod ui;
mod vcs;

use std::io::{self, Stdout, Write};
use std::sync::mpsc;
use std::thread;

use anyhow::Result;
use crossterm::event::{
    self as crossterm_event, DisableBracketedPaste, EnableBracketedPaste, Event,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::action::PromptKind;
use crate::app::App;
use crate::config::CursorShape;
use crate::finder::{FuzzyKind, IgnoreOpts};

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    // `vorto grammar …` is a one-shot CLI that builds and installs
    // tree-sitter `.so` libraries; it never enters the TUI, so handle
    // it before we touch the terminal.
    if argv.get(1).map(String::as_str) == Some("grammar") {
        return grammar::cli::run(&argv[2..]);
    }
    // `--version` / `--help` are likewise one-shots — print and exit
    // before any terminal setup.
    match argv.get(1).map(String::as_str) {
        Some("-V" | "--version") => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("-h" | "--help") => {
            print_usage();
            return Ok(());
        }
        _ => {}
    }

    let path = argv.into_iter().nth(1);
    // Anchor for LSP workspace root discovery — captured once here so the
    // value can't shift mid-session if anything changes the process's
    // cwd. Every later `:e` resolves against the same directory.
    let mut startup_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    // `vorto <dir>` (e.g. `vorto .`) means "open this directory as the
    // workspace root" — not "load the directory as a file" (which would
    // EISDIR out of `read_to_string`). Treat the arg as a workspace
    // anchor and open the fuzzy file picker instead.
    let (file_arg, dir_arg) = match path {
        Some(p) => {
            let pb = std::path::PathBuf::from(&p);
            if pb.is_dir() {
                let abs = if pb.is_absolute() {
                    pb.clone()
                } else {
                    startup_cwd.join(&pb)
                };
                let canon = abs.canonicalize().unwrap_or(abs);
                // chdir so child processes spawned by LSP / git inherit
                // the workspace root, matching what a user `cd` would do.
                let _ = std::env::set_current_dir(&canon);
                startup_cwd = canon;
                (None, true)
            } else {
                (Some(p), false)
            }
        }
        None => (None, false),
    };

    log::init();
    vlog!(
        "startup pid={} version={} cwd={}",
        std::process::id(),
        env!("CARGO_PKG_VERSION"),
        startup_cwd.display(),
    );

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // Bracketed paste: the terminal wraps pasted text in `\x1b[200~` …
    // `\x1b[201~` so crossterm can surface it as `Event::Paste(String)`
    // instead of a stream of synthesized key events. Without this, every
    // `\n` in the paste fires the Enter handler and auto-indent compounds
    // on the indent the pasted text already carries.
    execute!(stdout, EnableBracketedPaste)?;
    // Kitty keyboard protocol: with `DISAMBIGUATE_ESCAPE_CODES`, the
    // terminal reports Shift+Tab, Ctrl+modified keys, etc. as distinct
    // events instead of collapsing them onto plain ASCII codes. Without
    // it, e.g. macOS Terminal.app sends Shift+Tab as plain Tab (no
    // SHIFT modifier), making it indistinguishable from Tab. Push only
    // on terminals that advertise support — pushing on an unsupported
    // terminal is usually harmless but `supports_keyboard_enhancement`
    // is the documented gate.
    let kbd_enhanced = supports_keyboard_enhancement().unwrap_or(false);
    vlog!("kbd_enhanced={kbd_enhanced}");
    if kbd_enhanced {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    // EnterAlternateScreen *should* clear the alt screen, but not every
    // terminal honors that. Without an explicit clear, stale cells from
    // a previous vorto session (or any program that ran in the same alt
    // buffer) can leak through anywhere our render doesn't write — and
    // ratatui's diff won't fix them, because the previous-buffer it
    // diffs against is its own empty buffer, not the terminal's actual
    // contents. Force a full clear so the next draw paints onto a known
    // blank screen.
    terminal.clear()?;

    let cfg_path = config::default_path();
    let cfg = match config::Config::load(cfg_path.as_deref()) {
        Ok(c) => {
            vlog!(
                "config loaded path={}",
                cfg_path
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<default>".into()),
            );
            c
        }
        Err(e) => {
            vlog!("config load failed: {e:#}");
            return Err(e);
        }
    };
    let loader = syntax::Loader::new(cfg.grammar_dir.clone(), cfg.query_dir.clone());

    // Unified event channel. Terminal input runs on a dedicated thread
    // that pushes `Event::Term`; LSP reader threads push `Event::Lsp`.
    let (event_tx, event_rx) = mpsc::channel::<event::AppEvent>();
    let input_tx = event_tx.clone();
    thread::spawn(move || loop {
        match crossterm_event::read() {
            Ok(ev) => {
                if input_tx.send(event::AppEvent::Term(ev)).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    });

    let mut app = App::new(cfg, loader, event_tx, startup_cwd);
    // Best-effort: spawn Copilot eagerly so ghost-text completions are
    // ready by the time the user starts typing. Silent no-op when the
    // server binary isn't installed.
    app.spawn_copilot_if_needed();
    if let Some(p) = file_arg {
        app.open_path(std::path::Path::new(&p))?;
    } else if dir_arg {
        app.open_prompt(PromptKind::Fuzzy(FuzzyKind::Files {
            ignore: IgnoreOpts::DEFAULT,
        }));
    }

    let result = run(&mut terminal, &mut app, &event_rx);

    disable_raw_mode()?;
    let _ = execute!(terminal.backend_mut(), DisableBracketedPaste);
    if kbd_enhanced {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
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
    let mut prev_prompt_open = false;
    while !app.should_quit {
        app.buffer.refresh_highlights();
        app.tick_toasts();
        // When any modal prompt (fuzzy picker, hover popup, completion,
        // …) just closed, force a full repaint of the next frame. The
        // popup widgets only `Clear` their own rect, so cells the popup
        // wrote that *aren't* covered by the post-close render would
        // otherwise rely on ratatui's per-cell diff to clean them up.
        // That has been observed to leak syntax-highlighted fragments
        // when a fuzzy preview disappears, presumably because of a
        // diff-vs-terminal-state mismatch the previous-buffer doesn't
        // catch. `terminal.clear()` resets the back buffer so the next
        // diff emits every cell, masking the issue.
        let now_open = app.prompt.is_open();
        if prev_prompt_open && !now_open {
            terminal.clear()?;
        }
        prev_prompt_open = now_open;
        terminal.draw(|f| ui::draw(f, app))?;
        let shape = app.config.cursor_shapes.for_mode(app.mode);
        if last_shape != Some(shape) {
            let mut out = io::stdout();
            out.write_all(cursor_ansi(shape, app.config.cursor_shapes.blinking))?;
            out.flush()?;
            last_shape = Some(shape);
        }
        // Block on the next event. Both terminal input and LSP reader
        // threads feed this channel, so we wake on whichever comes first
        // and only redraw once after we drain the burst.
        //
        // When a toast is on screen, fall back to `recv_timeout` so the
        // loop wakes when the TTL expires and the next redraw can drop
        // the toast — otherwise it would linger until the user happens
        // to press a key.
        // Merge wake sources: toast TTL and indent-guide animation.
        // Smallest non-`None` wins; `None`-vs-`None` falls back to a
        // blocking `recv`.
        let wake = match (app.toast_remaining(), app.indent_anim_remaining()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let first = match wake {
            Some(rem) => match event_rx.recv_timeout(rem) {
                Ok(ev) => Some(ev),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            },
            None => match event_rx.recv() {
                Ok(ev) => Some(ev),
                Err(_) => return Ok(()),
            },
        };
        if let Some(ev) = first {
            dispatch(app, ev)?;
        }
        // Drain any events that piled up while we were blocked so we
        // don't redraw between a Term+Lsp pair (e.g. didChange burst).
        while let Ok(ev) = event_rx.try_recv() {
            dispatch(app, ev)?;
        }
        app.sync_buffer_if_dirty();
    }
    Ok(())
}

/// Append a single key event to the path in `VORTO_KEY_LOG`, if that
/// env var is set. For diagnosing terminals that swallow or remap keys
/// like Shift+Tab. Errors are silently dropped — this is opt-in debug.
fn log_key_event(key: &crossterm_event::KeyEvent) {
    use std::fs::OpenOptions;
    let Ok(path) = std::env::var("VORTO_KEY_LOG") else {
        return;
    };
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{key:?}");
    }
}

fn dispatch(app: &mut App, ev: event::AppEvent) -> Result<()> {
    match ev {
        event::AppEvent::Term(Event::Key(key)) => {
            log_key_event(&key);
            app.handle_key(key)?;
        }
        event::AppEvent::Term(Event::Paste(s)) => app.handle_paste(s),
        event::AppEvent::Term(_) => {}
        event::AppEvent::Lsp(lsp_ev) => app.handle_lsp_event(lsp_ev),
        event::AppEvent::Copilot(cp_ev) => app.handle_copilot_event(cp_ev),
        event::AppEvent::CopilotReady { result } => app.handle_copilot_ready(result),
        event::AppEvent::HighlighterReady { generation, result } => {
            app.handle_highlighter_ready(generation, result);
        }
        event::AppEvent::LspReady {
            generation,
            client_key,
            lang,
            path,
            result,
        } => {
            app.handle_lsp_ready(generation, client_key, lang, path, result);
        }
        event::AppEvent::PreviewReady(entry) => app.handle_preview_ready(entry),
    }
    Ok(())
}

fn print_usage() {
    println!(
        "{name} {version}

Usage:
    vorto [FILE|DIR]
    vorto grammar <list|install|remove> [args]
    vorto -h | --help
    vorto -V | --version",
        name = env!("CARGO_PKG_NAME"),
        version = env!("CARGO_PKG_VERSION"),
    );
}

/// DECSCUSR escape sequence — `CSI Ps SP q`, where Ps picks the shape.
/// Written directly to stdout from the main loop so the terminal
/// switches shape as the user changes mode.
fn cursor_ansi(shape: CursorShape, blinking: bool) -> &'static [u8] {
    match (shape, blinking) {
        (CursorShape::Terminal, _) => b"\x1b[0 q",
        (CursorShape::Block, true) => b"\x1b[1 q",
        (CursorShape::Block, false) => b"\x1b[2 q",
        (CursorShape::Underbar, true) => b"\x1b[3 q",
        (CursorShape::Underbar, false) => b"\x1b[4 q",
        (CursorShape::Bar, true) => b"\x1b[5 q",
        (CursorShape::Bar, false) => b"\x1b[6 q",
    }
}