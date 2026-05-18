//! Keyboard input dispatch.
//!
//! [`App::handle_key`] is the entry point. It routes to the prompt /
//! jump overlay first, then to the per-mode handlers in [`insert`],
//! [`visual`], and [`prompt`]. Normal-mode input flows through the token
//! pipeline in [`crate::app::eval`]; this module's role is everything
//! that *isn't* the Normal-mode operator/motion grammar.
//!
//! Mode-boundary book-keeping (visual anchor, cursor clamping) and the
//! prompt-opening helpers live here too, since they're called from
//! both the eval pipeline and the per-mode handlers.

mod insert;
mod prompt;
mod visual;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::action::PromptKind;
use crate::finder::{FuzzyKind, IgnoreOpts, workspace_files};
use crate::lsp::{Location, Position, Range, Severity, path_to_uri, uri_to_path};
use crate::mode::Mode;

use crate::buffer_ref::BufferRef;

use super::{App, eval};

impl App {
    /// Bracketed-paste payload from the terminal. Routed past the
    /// per-key dispatchers because we want the text inserted verbatim:
    /// no auto-indent on `\n`, no auto-pair on `(`, no completion
    /// triggers on identifier chars. Each context has its own destination:
    ///   - Prompt open: feed each char into the prompt's line input,
    ///     dropping line breaks (the prompt is single-line).
    ///   - Insert mode: splice the text in via `insert_text_raw` and
    ///     record it as one `InsertKey::Paste` for `.` replay.
    ///   - Normal / Visual: ignore — vim has no built-in "paste here"
    ///     gesture without an explicit register, and synthesizing one
    ///     would surprise the user.
    pub fn handle_paste(&mut self, s: String) {
        if self.prompt.is_open() {
            let filtered: String = s.chars().filter(|&c| c != '\n' && c != '\r').collect();
            for c in filtered.chars() {
                let ev = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
                let _ = self.handle_prompt_key(ev);
            }
            return;
        }
        if matches!(self.mode, Mode::Insert) {
            self.insert_pasted_text(s);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.prompt.is_open() {
            return self.handle_prompt_key(key);
        }

        // `gw` overlay swallows every key until the user picks a label
        // or cancels. Sits above the panic-button to keep Esc / Ctrl-C
        // local to the overlay (they cancel jump, not the whole app).
        if self.jump_state.is_some() {
            self.handle_jump_key(key);
            return Ok(());
        }

        // Global panic button.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Ok(());
        }

        // Esc in Normal mode dismisses a sticky fatal toast before
        // anything else. Pre-existing handlers (jump overlay, prompt)
        // already ran above, so this only fires when the editor is
        // genuinely idle. Other modes leave the toast alone — the user
        // is in the middle of input and shouldn't have side effects on
        // mode-exit Esc.
        if matches!(self.mode, Mode::Normal) && key.code == KeyCode::Esc && self.toasts.has_fatal()
        {
            self.toasts.dismiss_fatal();
            return Ok(());
        }

        // Insert & Visual modes have small enough surfaces that they're
        // handled directly. The token pipeline is Normal-mode only — that
        // is where the rich operator/motion/text-object grammar lives.
        match self.mode {
            Mode::Insert => return self.handle_insert_key(key),
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
                return self.handle_visual_key(key);
            }
            Mode::Normal => {}
        }

        // Normal mode: tokenize → classify → evaluate.
        match eval::tokenize(&self.config.keymap, &self.tokens, self.mode, key) {
            Some(t) => self.tokens.push(t),
            None => {
                self.tokens.clear();
                return Ok(());
            }
        }
        match eval::classify(&self.tokens) {
            eval::Parse::Complete(expr) => {
                self.tokens.clear();
                self.evaluate(expr, crate::action::Ctx::default())?;
            }
            eval::Parse::Incomplete => {}
            eval::Parse::Invalid => self.tokens.clear(),
        }
        Ok(())
    }

    pub(in crate::app) fn enter_mode(&mut self, mode: Mode) {
        // Set or clear the visual anchor at the mode boundary. Entering
        // any visual mode pins the anchor to the current cursor;
        // entering Normal/Insert drops it.
        if mode.is_visual() && !self.mode.is_visual() {
            self.visual_anchor = Some(self.buffer.cursor);
        } else if !mode.is_visual() {
            self.visual_anchor = None;
        }
        if mode == Mode::Normal {
            self.buffer.clamp_col(false);
        }
        self.mode = mode;
    }

    pub fn open_prompt(&mut self, kind: PromptKind) {
        match kind {
            PromptKind::Command => self.prompt.open_command(),
            PromptKind::Search { forward } => self.prompt.open_search(forward),
            PromptKind::Fuzzy(FuzzyKind::Files { ignore }) => {
                self.prompt.open_files(&self.startup_cwd, ignore)
            }
            PromptKind::Fuzzy(FuzzyKind::Lines) => self.prompt.open_lines(&self.buffer.lines),
            PromptKind::Fuzzy(FuzzyKind::Buffers) => self.open_buffer_picker(),
            // `Locations` pickers are built from server results, not opened
            // from a keymap — fall through to a no-op rather than a fresh
            // empty picker that would do nothing useful on submit.
            PromptKind::Fuzzy(FuzzyKind::Locations) => {}
            PromptKind::Fuzzy(FuzzyKind::WorkspaceSearch) => self.open_workspace_search(),
            PromptKind::Fuzzy(FuzzyKind::Diagnostics { workspace }) => {
                self.open_diagnostics_picker(workspace)
            }
        }
    }

    /// `<space>d` / `<space>D` — build the diagnostics picker.
    ///
    /// `workspace == false` lists every diagnostic for the current
    /// buffer (`[sev] line  message`); `workspace == true` folds in
    /// every URI the coordinator has diagnostics for and prefixes
    /// each row with the relative path (`[sev] path:line  message`).
    /// Both go through the same `Location` side-channel as references,
    /// so submit fires `JumpToLocation`.
    fn open_diagnostics_picker(&mut self, workspace: bool) {
        let mut items: Vec<String> = Vec::new();
        let mut locations: Vec<Location> = Vec::new();
        let root = self.startup_cwd.clone();

        if workspace {
            for (uri, diags) in self.all_diagnostics() {
                let label = relative_uri_label(&uri, &root);
                for d in &diags {
                    items.push(format!(
                        "[{}] {}:{}",
                        severity_tag(d.severity),
                        label,
                        d.range.start.line + 1,
                    ));
                    locations.push(Location {
                        uri: uri.clone(),
                        range: d.range,
                    });
                }
            }
        } else {
            let uri = match self.buffer.path.as_ref().map(|p| path_to_uri(p)) {
                Some(u) => u,
                None => {
                    self.push_toast(crate::app::Toast::info("no diagnostics"));
                    return;
                }
            };
            let Some(diags) = self.current_diagnostics() else {
                self.push_toast(crate::app::Toast::info("no diagnostics"));
                return;
            };
            for d in &diags {
                items.push(format!(
                    "[{}] {}",
                    severity_tag(d.severity),
                    d.range.start.line + 1,
                ));
                locations.push(Location {
                    uri: uri.clone(),
                    range: d.range,
                });
            }
        }

        if items.is_empty() {
            self.push_toast(crate::app::Toast::info("no diagnostics"));
            return;
        }
        self.prompt.open_diagnostics(items, locations, workspace);
    }

    /// Build the MRU display list and open the buffer picker. Shows
    /// every recently-touched buffer, current one included, plus the
    /// scratch sentinel.
    ///
    /// Each entry carries three leading columns:
    ///   - `%` if it's the active buffer, otherwise blank.
    ///   - `~` if the file differs from HEAD (live diff for the
    ///     active buffer, `git status --porcelain` set for the rest).
    ///   - `+` if the buffer has unsaved edits.
    ///
    /// Always opens (even on empty MRU) so the user gets a visible
    /// "(no matches)" instead of silent nothing.
    fn open_buffer_picker(&mut self) {
        let cwd = &self.startup_cwd;
        let current_path = self
            .buffer
            .path
            .as_ref()
            .and_then(|p| p.canonicalize().ok());
        let on_scratch = self.buffer.path.is_none();
        let active_dirty = self.buffer.dirty;
        let active_vcs_changed = self.buffer.has_vcs_changes();
        // One `git status` invocation feeds the VCS marker for every
        // non-active File entry in the list — cheaper than diffing
        // each sleeping buffer individually.
        let vcs_set = crate::vcs::changed_files(cwd);

        let (items, refs): (Vec<_>, Vec<_>) = self
            .opened_paths
            .iter()
            .rev() // newest first
            .map(|r| {
                let (label, is_current) = match r {
                    BufferRef::Scratch(id) => {
                        let is_current = on_scratch && self.current_scratch_id == Some(*id);
                        (BufferRef::scratch_label(*id), is_current)
                    }
                    BufferRef::File(p) => {
                        let rel = p
                            .strip_prefix(cwd)
                            .unwrap_or(p)
                            .to_string_lossy()
                            .to_string();
                        let is_current = current_path.as_ref() == Some(p);
                        (rel, is_current)
                    }
                };
                // Dirty is tracked on whichever copy is live: the
                // active buffer for `is_current`, the sleeping map
                // entry for everything else.
                let entry_dirty = if is_current {
                    active_dirty
                } else {
                    self.sleeping.get(r).is_some_and(|b| b.dirty)
                };
                // VCS marker. Scratch never has a VCS state. For the
                // active File we trust the live in-memory diff (catches
                // unsaved edits that `git status` can't see); for every
                // other File we fall back to the porcelain set, then
                // OR in the unsaved-dirty bit so an inactive edited
                // buffer still shows as changed even if its on-disk
                // copy matches HEAD.
                let entry_vcs = match r {
                    BufferRef::Scratch(_) => false,
                    BufferRef::File(p) => {
                        if is_current {
                            active_vcs_changed
                        } else {
                            vcs_set.contains(p) || entry_dirty
                        }
                    }
                };
                let cur_col = if is_current { '%' } else { ' ' };
                let vcs_col = if entry_vcs { '~' } else { ' ' };
                let mod_col = if entry_dirty { '+' } else { ' ' };
                let display = format!("{}{}{} {}", cur_col, vcs_col, mod_col, label);
                (display, r.clone())
            })
            .unzip();
        self.prompt.open_buffers(items, refs);
    }

    /// `<space>/` — build a workspace-wide line picker.
    ///
    /// Reads every tracked text file (or, outside git, the manual
    /// walker's set), one `path:line: text` entry per line, and opens
    /// the picker. The fuzzy matcher then narrows in-memory as the
    /// user types, and submit jumps to the file/line via
    /// `JumpToLocation` (same outcome path as the LSP references
    /// picker — preview, jump-list bookkeeping, all free).
    ///
    /// Filtering, applied in order, to keep the candidate set small
    /// enough that per-keystroke refilter stays snappy:
    ///   - `.gitignore` + dotfiles excluded (the file walker handles
    ///     this via [`IgnoreOpts::DEFAULT`]).
    ///   - Only extensions in [`is_searchable_ext`] are considered —
    ///     so lockfiles, minified bundles, binaries, etc. don't blow
    ///     up the line count.
    ///   - Files larger than [`WORKSPACE_SEARCH_MAX_FILE_BYTES`] and
    ///     anything that doesn't decode as UTF-8 are skipped silently.
    ///   - Hard cap of [`WORKSPACE_SEARCH_MAX_LINES`] total entries.
    fn open_workspace_search(&mut self) {
        let cwd = &self.startup_cwd;
        let files = workspace_files(cwd, IgnoreOpts::DEFAULT);
        let mut items: Vec<String> = Vec::new();
        let mut file_lines: Vec<Vec<String>> = Vec::new();
        let mut locations: Vec<Location> = Vec::new();
        let mut total_lines: usize = 0;
        for rel in files {
            if total_lines >= WORKSPACE_SEARCH_MAX_LINES {
                break;
            }
            if !is_searchable_ext(&rel) {
                continue;
            }
            let abs = cwd.join(&rel);
            let meta = match std::fs::metadata(&abs) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.len() > WORKSPACE_SEARCH_MAX_FILE_BYTES {
                continue;
            }
            let content = match std::fs::read_to_string(&abs) {
                Ok(s) => s,
                // Binary or non-UTF8 — skip rather than surface garbled
                // bytes.
                Err(_) => continue,
            };
            // Keep one slot per line so MatchItem::line_hits indexes
            // line up with on-disk row numbers (empty lines included).
            let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
            if lines.is_empty() {
                continue;
            }
            total_lines += lines.len();
            items.push(rel.clone());
            file_lines.push(lines);
            locations.push(Location {
                uri: path_to_uri(&abs),
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 0,
                    },
                },
            });
        }
        self.prompt
            .open_workspace_search(items, file_lines, locations);
    }
}

/// Render the `path` portion of a workspace-diagnostic entry. Strips
/// the workspace root and canonicalises both sides so symlinked paths
/// don't surface as absolute. Falls back to the raw URI when the
/// scheme isn't `file://`.
fn relative_uri_label(uri: &str, root: &std::path::Path) -> String {
    let Some(path) = uri_to_path(uri) else {
        return uri.to_string();
    };
    let path_c = path.canonicalize().unwrap_or_else(|_| path.clone());
    let root_c = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    path_c
        .strip_prefix(&root_c)
        .unwrap_or(&path_c)
        .to_string_lossy()
        .into_owned()
}

/// One-letter severity badge for the picker — `E` / `W` / `I` / `H`.
fn severity_tag(sev: Severity) -> char {
    match sev {
        Severity::Error => 'E',
        Severity::Warning => 'W',
        Severity::Info => 'I',
        Severity::Hint => 'H',
    }
}

/// Hard cap on the number of candidate lines fed to the fuzzy matcher
/// for `<space>/`. The matcher runs O(items * query) on every
/// keystroke, so a soft ceiling here keeps typing latency bounded in
/// huge repos.
const WORKSPACE_SEARCH_MAX_LINES: usize = 50_000;

/// Skip individual files larger than this. Catches generated lockfiles,
/// vendored bundles, etc. that would dominate the candidate list with
/// content the user almost never wants to search.
const WORKSPACE_SEARCH_MAX_FILE_BYTES: u64 = 1_000_000;

/// True if `rel` looks like a source/text file the user is likely to
/// want to grep. Extension allowlist rather than a binary denylist
/// because the search input is the *content* of every line we collect
/// — we'd rather skip an obscure plaintext format than include
/// `package-lock.json` and watch the picker stall on every keystroke.
///
/// `Makefile`, `Dockerfile`, etc. (no extension) are accepted by
/// matching common basenames.
fn is_searchable_ext(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    if matches!(
        name,
        "Makefile" | "Dockerfile" | "Justfile" | "CMakeLists.txt" | "Cargo.toml" | "Cargo.lock"
    ) {
        // Cargo.lock is intentionally in: it's small enough on most
        // repos, and users do occasionally search for versions in it.
        // The size cap catches the pathological case.
        return true;
    }
    let Some(ext) = name.rsplit_once('.').map(|(_, e)| e) else {
        return false;
    };
    matches!(
        ext,
        // Systems
        "rs" | "go" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx"
        | "zig" | "v" | "nim" | "d"
        // JVM
        | "java" | "kt" | "kts" | "scala" | "groovy" | "clj" | "cljs"
        // Apple
        | "swift" | "m" | "mm"
        // Scripting
        | "py" | "rb" | "php" | "pl" | "lua" | "tcl" | "r"
        | "sh" | "bash" | "zsh" | "fish"
        // Web
        | "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "vue" | "svelte"
        | "html" | "htm" | "xml" | "css" | "scss" | "sass" | "less" | "styl"
        // Functional
        | "hs" | "ml" | "mli" | "ex" | "exs" | "erl" | "elm" | "fs" | "fsx"
        // Configs / data
        | "toml" | "yaml" | "yml" | "json" | "jsonc" | "ron" | "ini" | "conf"
        | "env" | "properties"
        // Docs / plain text
        | "md" | "mdx" | "rst" | "adoc" | "txt" | "tex"
        // Build / query
        | "mk" | "cmake" | "ninja" | "bazel" | "bzl" | "gradle"
        | "sql" | "graphql" | "gql" | "proto"
    )
}
