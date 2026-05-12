//! Owns LSP client state and the request/response bookkeeping.
//!
//! `App` resolves a language and asks the coordinator to attach / sync /
//! request things. The coordinator drives the wire-level protocol and
//! reports back via [`LspEventOutcome`] — App turns outcomes into
//! user-visible side effects (status messages, file opens, buffer edits).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};
use serde_json::Value;

use super::{self as lsp, Diagnostic, Location, LspClient, LspEvent, TextEdit, WorkspaceEdit};
use crate::config::LspConfig;
use crate::editor::Cursor;
use crate::event::AppEvent;

/// What an outstanding LSP request was for. Stored under
/// `pending[(lang, id)]` and consumed when the matching
/// [`LspEvent::Response`] arrives.
#[derive(Debug, Clone)]
pub enum LspRequestKind {
    /// Any `Location[]`-shaped jump request — `definition`,
    /// `declaration`, `implementation`. Only the user-visible label
    /// is needed for the "no results" status message.
    Jump { label: &'static str },
    /// `textDocument/references` — show the locations in a picker.
    References,
    /// `textDocument/rename` — apply the returned `WorkspaceEdit`.
    /// `new_name` is kept for the post-apply status line.
    Rename { new_name: String },
}

/// What [`LspCoordinator::handle_event`] wants the caller to do.
/// Diagnostics events are absorbed internally; everything that requires
/// UI action surfaces here.
pub enum LspEventOutcome {
    /// No user-visible side effect required.
    Nothing,
    InfoMessage(String),
    ErrorMessage(String),
    /// Jump-style response: caller should open the first location.
    Jump {
        label: &'static str,
        locations: Vec<Location>,
    },
    /// References response: caller picks single-jump vs picker.
    References(Vec<Location>),
    /// Rename response: caller applies the edit (or shows "nothing to change").
    Rename {
        new_name: String,
        edit: Option<WorkspaceEdit>,
    },
}

/// Result of applying a [`WorkspaceEdit`]. Other-file edits are written
/// to disk by the coordinator; the active buffer's edits are returned
/// for the caller to apply through its own `Buffer` (with undo, version
/// bump, etc.) — keeping buffer mutation out of the coordinator.
pub struct WorkspaceEditResult {
    pub current_buffer_edits: Vec<TextEdit>,
    pub files_touched: usize,
    pub total_edits: usize,
}

pub struct LspCoordinator {
    /// Live LSP clients, keyed by language name. Spawned lazily on the
    /// first `ensure_client` for a language with `[languages.<name>.lsp]`
    /// configured. The same client is reused across files of that
    /// language.
    clients: HashMap<String, LspClient>,
    /// Diagnostics keyed by URI. URIs are produced via `lsp::path_to_uri`
    /// so the lookup matches whatever the server reports back.
    diagnostics: HashMap<String, Vec<Diagnostic>>,
    /// Outstanding LSP request bookkeeping. Keyed by `(lang, id)` so a
    /// response arriving on the shared event channel can be routed back
    /// to the right handler regardless of which client sent it.
    pending: HashMap<(String, u64), LspRequestKind>,
    /// URI of the document currently considered "open" (cached so
    /// `didChange`/`didClose` don't re-canonicalise every time).
    current_uri: Option<String>,
    /// Language name of the currently-open document.
    current_language: Option<String>,
    /// Last buffer `version` we synced via `didChange`. Compared by
    /// `App` against the live buffer's version to decide whether to
    /// fire a sync.
    last_synced_version: u64,
    /// Sender shared with input + LSP reader threads. Cloned into each
    /// new LSP client at spawn time.
    event_tx: Sender<AppEvent>,
    /// Working directory captured at process startup. All workspace
    /// root discovery anchors here.
    startup_cwd: PathBuf,
}

impl LspCoordinator {
    pub fn new(event_tx: Sender<AppEvent>, startup_cwd: PathBuf) -> Self {
        Self {
            clients: HashMap::new(),
            diagnostics: HashMap::new(),
            pending: HashMap::new(),
            current_uri: None,
            current_language: None,
            last_synced_version: 0,
            event_tx,
            startup_cwd,
        }
    }

    pub fn last_synced_version(&self) -> u64 {
        self.last_synced_version
    }

    pub fn set_last_synced_version(&mut self, v: u64) {
        self.last_synced_version = v;
    }

    pub fn has_lsp(&self) -> bool {
        match (&self.current_uri, &self.current_language) {
            (Some(_), Some(lang)) => self.clients.contains_key(lang),
            _ => false,
        }
    }

    /// Diagnostics for the current buffer's URI, if any.
    pub fn current_diagnostics(&self) -> Option<&[Diagnostic]> {
        self.current_uri
            .as_ref()
            .and_then(|u| self.diagnostics.get(u))
            .map(|v| v.as_slice())
    }

    /// First diagnostic that covers the given row, prioritising errors.
    pub fn diagnostic_on_cursor(&self, row: u32) -> Option<&Diagnostic> {
        let mut best: Option<&Diagnostic> = None;
        for d in self.current_diagnostics()? {
            if d.range.start.line <= row && row <= d.range.end.line {
                best = Some(match best {
                    None => d,
                    Some(prev) if (d.severity as u8) < (prev.severity as u8) => d,
                    Some(prev) => prev,
                });
            }
        }
        best
    }

    /// Tell the current document's LSP that we're done with it. No-op
    /// when there's no current document.
    pub fn detach_current(&mut self) {
        let (Some(uri), Some(lang)) = (self.current_uri.take(), self.current_language.take())
        else {
            return;
        };
        if let Some(client) = self.clients.get_mut(&lang) {
            let _ = client.did_close(&uri);
        }
    }

    /// Ensure a client for `lang_name` is running. Spawns one if not.
    /// On failure, returns the spawn error — caller decides whether to
    /// silence "command not found" errors.
    pub fn ensure_client(
        &mut self,
        lang_name: &str,
        lsp_cfg: &LspConfig,
        path: &Path,
    ) -> Result<()> {
        if self.clients.contains_key(lang_name) {
            return Ok(());
        }
        let root_dir = lsp::discover_root(&self.startup_cwd, Some(path), &lsp_cfg.root_markers);
        let root_uri = lsp::path_to_uri(&root_dir);
        let tx = self.event_tx.clone();
        let emit: Box<dyn Fn(LspEvent) + Send> = Box::new(move |ev| {
            let _ = tx.send(AppEvent::Lsp(ev));
        });
        let client = LspClient::spawn(lang_name, lsp_cfg, &root_uri, emit)?;
        self.clients.insert(lang_name.to_string(), client);
        Ok(())
    }

    /// Send `didOpen` for `path` against the client for `lang_name`.
    /// Sets the document as the current one on success. No-op when the
    /// client is missing.
    pub fn did_open(&mut self, lang_name: &str, path: &Path, text: &str) -> Result<()> {
        let uri = lsp::path_to_uri(path);
        if let Some(client) = self.clients.get_mut(lang_name) {
            client.did_open(&uri, text)?;
        }
        self.current_uri = Some(uri);
        self.current_language = Some(lang_name.to_string());
        Ok(())
    }

    /// Send `didChange` for the current document. No-op when no
    /// document or client is active.
    pub fn did_change(&mut self, text: &str) -> Result<()> {
        let (Some(uri), Some(lang)) = (&self.current_uri, &self.current_language) else {
            return Ok(());
        };
        let Some(client) = self.clients.get_mut(lang) else {
            return Ok(());
        };
        client.did_change(uri, text)
    }

    /// Send `didSave` for the current document. No-op when no
    /// document or client is active.
    pub fn did_save(&mut self, text: &str) -> Result<()> {
        let (Some(uri), Some(lang)) = (&self.current_uri, &self.current_language) else {
            return Ok(());
        };
        let Some(client) = self.clients.get_mut(lang) else {
            return Ok(());
        };
        client.did_save(uri, text)
    }

    /// `textDocument/definition`-style request. `method` is the
    /// concrete LSP method; `label` is the user-visible noun for
    /// status messages.
    pub fn request_jump(
        &mut self,
        method: &str,
        label: &'static str,
        cursor: Cursor,
    ) -> Result<()> {
        let params = self.text_document_position_params(cursor);
        self.send_request(method, params, LspRequestKind::Jump { label })
    }

    pub fn request_references(&mut self, cursor: Cursor) -> Result<()> {
        let mut params = self.text_document_position_params(cursor);
        if let Some(obj) = params.as_object_mut() {
            obj.insert(
                "context".to_string(),
                serde_json::json!({ "includeDeclaration": true }),
            );
        }
        self.send_request(
            "textDocument/references",
            params,
            LspRequestKind::References,
        )
    }

    pub fn request_rename(&mut self, new_name: String, cursor: Cursor) -> Result<()> {
        let mut params = self.text_document_position_params(cursor);
        if let Some(obj) = params.as_object_mut() {
            obj.insert("newName".to_string(), Value::String(new_name.clone()));
        }
        self.send_request(
            "textDocument/rename",
            params,
            LspRequestKind::Rename { new_name },
        )
    }

    fn text_document_position_params(&self, cursor: Cursor) -> Value {
        let uri = self.current_uri.clone().unwrap_or_default();
        serde_json::json!({
            "textDocument": { "uri": uri },
            "position": {
                "line": cursor.row as u64,
                "character": cursor.col as u64,
            }
        })
    }

    fn send_request(&mut self, method: &str, params: Value, kind: LspRequestKind) -> Result<()> {
        let Some(lang) = self.current_language.clone() else {
            return Ok(());
        };
        let Some(client) = self.clients.get_mut(&lang) else {
            return Ok(());
        };
        let id = client.request(method, params)?;
        self.pending.insert((lang, id), kind);
        Ok(())
    }

    /// Consume an LSP event. Diagnostics / messages are absorbed
    /// here; responses are routed back to their pending kind and
    /// surfaced as an [`LspEventOutcome`] for the caller to act on.
    pub fn handle_event(&mut self, ev: LspEvent) -> LspEventOutcome {
        match ev {
            LspEvent::Diagnostics { uri, items } => {
                if items.is_empty() {
                    self.diagnostics.remove(&uri);
                } else {
                    self.diagnostics.insert(uri, items);
                }
                LspEventOutcome::Nothing
            }
            LspEvent::Message { level, text } => {
                // Levels: 1 Error, 2 Warning, 3 Info, 4 Log.
                if level == 1 {
                    LspEventOutcome::ErrorMessage(text)
                } else {
                    LspEventOutcome::InfoMessage(text)
                }
            }
            LspEvent::Error(e) => LspEventOutcome::ErrorMessage(format!("lsp: {}", e)),
            LspEvent::Response {
                lang,
                id,
                result,
                error,
            } => {
                let Some(kind) = self.pending.remove(&(lang, id)) else {
                    return LspEventOutcome::Nothing;
                };
                if let Some(msg) = error {
                    return LspEventOutcome::ErrorMessage(format!("lsp: {}", msg));
                }
                let result = result.unwrap_or(Value::Null);
                match kind {
                    LspRequestKind::Jump { label } => LspEventOutcome::Jump {
                        label,
                        locations: lsp::parse_locations(&result),
                    },
                    LspRequestKind::References => {
                        LspEventOutcome::References(lsp::parse_locations(&result))
                    }
                    LspRequestKind::Rename { new_name } => LspEventOutcome::Rename {
                        new_name,
                        edit: lsp::parse_workspace_edit(&result),
                    },
                }
            }
        }
    }

    /// Apply a [`WorkspaceEdit`]: write other-file edits to disk,
    /// return the edits that target the active buffer so the caller
    /// can apply them through its own buffer machinery.
    pub fn apply_workspace_edit(&self, edit: WorkspaceEdit) -> Result<WorkspaceEditResult> {
        let mut current_buffer_edits = Vec::new();
        let files_touched = edit.changes.len();
        let mut total_edits = 0usize;
        let current_uri = self.current_uri.clone();
        for (uri, edits) in edit.changes {
            total_edits += edits.len();
            if Some(&uri) == current_uri.as_ref() {
                current_buffer_edits = edits;
                continue;
            }
            let Some(path) = lsp::uri_to_path(&uri) else {
                continue;
            };
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
            if lines.is_empty() {
                lines.push(String::new());
            }
            lsp::apply_text_edits(&mut lines, edits);
            std::fs::write(&path, lines.join("\n"))
                .with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(WorkspaceEditResult {
            current_buffer_edits,
            files_touched,
            total_edits,
        })
    }
}
