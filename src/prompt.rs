//! Owns the bottom-line prompt (`:cmd`, `/search`, fuzzy pickers,
//! rename input) and translates key events into outcomes the App
//! reacts to.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::buffer_ref::BufferRef;
use crate::finder::{Finder, FuzzyKind, IgnoreOpts};
use crate::lsp::{CodeAction, Location};

/// Single-line text input with a movable insertion point. `cursor` is a
/// char index in `[0, char_count]`; methods keep it in that range and
/// operate at char boundaries so multi-byte input behaves correctly.
#[derive(Default)]
pub struct LineInput {
    buf: String,
    cursor: usize,
}

impl LineInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn as_str(&self) -> &str {
        &self.buf
    }

    /// Char index of the insertion point.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn char_len(&self) -> usize {
        self.buf.chars().count()
    }

    fn byte_idx(&self, char_idx: usize) -> usize {
        self.buf
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.buf.len())
    }

    pub fn insert(&mut self, c: char) {
        let byte = self.byte_idx(self.cursor);
        self.buf.insert(byte, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_idx(self.cursor);
        let start = self.byte_idx(self.cursor - 1);
        self.buf.replace_range(start..end, "");
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.char_len() {
            return;
        }
        let start = self.byte_idx(self.cursor);
        let end = self.byte_idx(self.cursor + 1);
        self.buf.replace_range(start..end, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.char_len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.char_len();
    }

    pub fn into_string(self) -> String {
        self.buf
    }
}

/// Apply a single key event to a [`LineInput`]. Handles the standard
/// readline-ish bindings the user already expects in `:`, `/`, rename,
/// and the fuzzy picker query (left/right, home/end, Ctrl-A/E/B/F,
/// backspace/delete, plain char insertion).
pub(crate) fn apply_line_key(input: &mut LineInput, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        KeyCode::Home => input.home(),
        KeyCode::End => input.end(),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Char('b') if ctrl => input.left(),
        KeyCode::Char('f') if ctrl => input.right(),
        KeyCode::Char('a') if ctrl => input.home(),
        KeyCode::Char('e') if ctrl => input.end(),
        KeyCode::Char(c) if !ctrl => input.insert(c),
        _ => {}
    }
}

/// Active prompt state. Mirrors the four ways the user can interact
/// with the bottom-line input: `:` command line, `/` (or `?`) search,
/// fuzzy pickers, and rename.
pub enum Prompt {
    None,
    Command(LineInput),
    Search {
        forward: bool,
        query: LineInput,
    },
    Fuzzy(Finder),
    /// `<space>r` — text input for the new identifier. The cursor and
    /// URI captured at open-time aren't stored here: the LSP rename
    /// request is built against the live cursor at submit, which
    /// matches what the user sees (the cursor is locked while the
    /// prompt is up because Normal-mode input is suspended).
    Rename(LineInput),
    /// `<space>a` — popup menu of LSP code actions, anchored just under
    /// the buffer cursor. Up/Down navigate, Enter submits, Esc cancels.
    /// Filtering is intentionally omitted: action lists are short and
    /// users want to read titles, not type query strings.
    CodeActionMenu {
        actions: Vec<CodeAction>,
        selected: usize,
    },
    /// `K` — read-only popup showing `textDocument/hover` content
    /// anchored at the cursor. j/k/Up/Down/PageUp/PageDown scroll the
    /// content; any other key (including Enter and Esc) closes it.
    Hover {
        content: String,
        scroll: usize,
    },
}

impl Prompt {
    pub fn is_open(&self) -> bool {
        !matches!(self, Prompt::None)
    }
}

/// What a key event produced. `Nothing` means "input absorbed, prompt
/// stays open"; everything else closes the prompt and asks the caller
/// to act.
pub enum PromptOutcome {
    Nothing,
    /// User pressed Esc / Ctrl-C — prompt closed, no action.
    Cancelled,
    /// `:cmd` submitted. Caller parses and dispatches.
    RunCommand(String),
    /// `/` or `?` submitted.
    Search {
        forward: bool,
        query: String,
    },
    /// Fuzzy file picker submission. The path is relative to
    /// `startup_cwd` — re-anchored by the caller.
    OpenRelativeFile(String),
    /// Fuzzy line picker submission. 0-based row in the active buffer.
    GotoLine(usize),
    /// Fuzzy references picker submission.
    JumpToLocation(Location),
    /// Fuzzy buffer picker submission. The caller maps the
    /// [`BufferRef`] back to an actual buffer load (`Scratch` →
    /// fresh empty buffer, `File(path)` → `open_path`).
    OpenBuffer(BufferRef),
    /// Rename submitted with the new identifier.
    SubmitRename(String),
    /// Code action picker selection. The caller either applies the
    /// embedded `WorkspaceEdit` or sends a `codeAction/resolve` round
    /// trip first when `edit` is `None`.
    SelectCodeAction(CodeAction),
}

pub struct PromptController {
    pub state: Prompt,
    /// Side-channel for `Fuzzy(Locations)` pickers — `locations[idx]`
    /// matches the picker's `items[idx]`. Cleared on submit or cancel.
    locations: Vec<Location>,
    /// Side-channel for `Fuzzy(Buffers)` pickers — `buffer_paths[idx]`
    /// is the buffer to open when the user submits the matching item.
    /// Cleared on submit or cancel.
    buffer_paths: Vec<BufferRef>,
}

impl PromptController {
    pub fn new() -> Self {
        Self {
            state: Prompt::None,
            locations: Vec::new(),
            buffer_paths: Vec::new(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.state.is_open()
    }

    /// Side-channel `Location`s that mirror the active `Locations` picker.
    /// Returns `&[]` for any other prompt state. The UI uses this to read
    /// `locations[idx]` for preview rendering.
    pub fn locations(&self) -> &[Location] {
        &self.locations
    }

    pub fn open_command(&mut self) {
        self.state = Prompt::Command(LineInput::new());
    }

    pub fn open_search(&mut self, forward: bool) {
        self.state = Prompt::Search {
            forward,
            query: LineInput::new(),
        };
    }

    pub fn open_files(&mut self, startup_cwd: &Path, ignore: IgnoreOpts) {
        self.state = Prompt::Fuzzy(Finder::files(startup_cwd, ignore));
    }

    pub fn open_lines(&mut self, lines: &[String]) {
        self.state = Prompt::Fuzzy(Finder::lines(lines));
    }

    pub fn open_locations(&mut self, items: Vec<String>, locations: Vec<Location>) {
        self.locations = locations;
        self.state = Prompt::Fuzzy(Finder::locations(items));
    }

    /// Open a fuzzy buffer picker. `items` are the display strings;
    /// `refs` are the matching [`BufferRef`]s in parallel order —
    /// the controller stores them and produces an `OpenBuffer(…)`
    /// outcome on submit.
    pub fn open_buffers(&mut self, items: Vec<String>, refs: Vec<BufferRef>) {
        self.buffer_paths = refs;
        self.state = Prompt::Fuzzy(Finder::buffers(items));
    }

    /// Read-only view of the buffer-picker side-channel, mirroring
    /// [`Self::locations`]. The UI uses this for preview rendering.
    pub fn buffer_paths(&self) -> &[BufferRef] {
        &self.buffer_paths
    }

    pub fn open_rename(&mut self) {
        self.state = Prompt::Rename(LineInput::new());
    }

    /// Open the cursor-anchored code-actions popup. `actions` is consumed
    /// — we own them while the menu is up so submit can hand a fully-
    /// owned `CodeAction` to the caller without an extra clone.
    pub fn open_code_actions(&mut self, actions: Vec<CodeAction>) {
        self.state = Prompt::CodeActionMenu {
            actions,
            selected: 0,
        };
    }

    /// Open a hover popup with the given content. Cursor position is
    /// captured by the renderer at draw time, so `App` doesn't need to
    /// store it.
    pub fn open_hover(&mut self, content: String) {
        self.state = Prompt::Hover { content, scroll: 0 };
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PromptOutcome {
        let ctrl_c =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if key.code == KeyCode::Esc || ctrl_c {
            self.close();
            return PromptOutcome::Cancelled;
        }
        if key.code == KeyCode::Enter {
            return self.submit();
        }

        match &mut self.state {
            Prompt::None => PromptOutcome::Nothing,
            Prompt::Command(input) | Prompt::Rename(input) => {
                apply_line_key(input, key);
                PromptOutcome::Nothing
            }
            Prompt::Search { query, .. } => {
                apply_line_key(query, key);
                PromptOutcome::Nothing
            }
            Prompt::Fuzzy(finder) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Up => finder.prev(),
                    KeyCode::Down => finder.next(),
                    KeyCode::Char('n') if ctrl => finder.next(),
                    KeyCode::Char('p') if ctrl => finder.prev(),
                    _ => finder.apply_line_key(key),
                }
                PromptOutcome::Nothing
            }
            Prompt::CodeActionMenu { actions, selected } => {
                let last = actions.len().saturating_sub(1);
                match key.code {
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                    KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        *selected = selected.saturating_sub(1)
                    }
                    KeyCode::Down => *selected = (*selected + 1).min(last),
                    KeyCode::Char('j') => *selected = (*selected + 1).min(last),
                    KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        *selected = (*selected + 1).min(last)
                    }
                    _ => {}
                }
                PromptOutcome::Nothing
            }
            Prompt::Hover { scroll, .. } => {
                // Read-only popup. Esc/Ctrl-C/Enter are intercepted by
                // the top of `handle_key`, so here we only see scroll
                // keys and "anything else" (which we treat as dismiss).
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        *scroll = scroll.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *scroll = scroll.saturating_add(1);
                    }
                    KeyCode::PageUp => {
                        *scroll = scroll.saturating_sub(5);
                    }
                    KeyCode::PageDown => {
                        *scroll = scroll.saturating_add(5);
                    }
                    _ => {
                        self.close();
                        return PromptOutcome::Cancelled;
                    }
                }
                PromptOutcome::Nothing
            }
        }
    }

    fn close(&mut self) {
        self.state = Prompt::None;
        self.locations.clear();
        self.buffer_paths.clear();
    }

    fn submit(&mut self) -> PromptOutcome {
        let prompt = std::mem::replace(&mut self.state, Prompt::None);
        match prompt {
            Prompt::None => PromptOutcome::Nothing,
            Prompt::Command(line) => {
                PromptOutcome::RunCommand(line.as_str().trim().to_string())
            }
            Prompt::Search { forward, query } => PromptOutcome::Search {
                forward,
                query: query.into_string(),
            },
            Prompt::Rename(new_name) => PromptOutcome::SubmitRename(new_name.into_string()),
            Prompt::Fuzzy(finder) => self.submit_fuzzy(finder),
            Prompt::CodeActionMenu {
                mut actions,
                selected,
            } => {
                if selected < actions.len() {
                    PromptOutcome::SelectCodeAction(actions.swap_remove(selected))
                } else {
                    PromptOutcome::Nothing
                }
            }
            // Hover is read-only — Enter just dismisses it.
            Prompt::Hover { .. } => PromptOutcome::Cancelled,
        }
    }

    fn submit_fuzzy(&mut self, finder: Finder) -> PromptOutcome {
        let Some(sel) = finder.selection() else {
            self.locations.clear();
            return PromptOutcome::Nothing;
        };
        match finder.kind {
            FuzzyKind::Files { .. } => PromptOutcome::OpenRelativeFile(finder.items[sel.idx].clone()),
            FuzzyKind::Lines => PromptOutcome::GotoLine(sel.idx),
            FuzzyKind::Locations => {
                let loc = self.locations.get(sel.idx).cloned();
                self.locations.clear();
                match loc {
                    Some(loc) => PromptOutcome::JumpToLocation(loc),
                    None => PromptOutcome::Nothing,
                }
            }
            FuzzyKind::Buffers => {
                let r = self.buffer_paths.get(sel.idx).cloned();
                self.buffer_paths.clear();
                match r {
                    Some(r) => PromptOutcome::OpenBuffer(r),
                    None => PromptOutcome::Nothing,
                }
            }
        }
    }
}

impl Default for PromptController {
    fn default() -> Self {
        Self::new()
    }
}
