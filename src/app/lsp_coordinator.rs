//! Owns LSP client state and the request/response bookkeeping.
//!
//! `App` resolves a language and asks the coordinator to attach / sync /
//! request things. The coordinator drives the wire-level protocol and
//! reports back via [`LspEventOutcome`] — App turns outcomes into
//! user-visible side effects (status messages, file opens, buffer edits).
//!
//! Multi-server support: a buffer can have more than one LSP attached
//! (e.g. `vtsls` + `typescript-language-server`). Each spawned client
//! gets a unique `client_key` of the form `"<lang>::<server-name>"`.
//! Outgoing requests fan out to every client active for the current
//! document, and responses are accumulated in a per-request `Group`
//! before a single merged [`LspEventOutcome`] is surfaced.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::editor::Cursor;
use crate::event::AppEvent;
use crate::lsp::{
    self, CodeAction, CompletionItem, Diagnostic, Hover, Location, LspClient, LspEvent,
    SignatureHelp, TextEdit, WorkspaceEdit,
};

use super::signature::SignatureTrigger;

/// Build the canonical client identifier from a language name and a
/// server name. The same recipe runs everywhere `client_key` is needed
/// so spawn / attach / lookup stay in lockstep.
pub fn client_key(lang: &str, server: &str) -> String {
    format!("{}::{}", lang, server)
}

/// What an outstanding LSP request was for. Stored under
/// `pending[(client_key, id)]` so a response on the shared event
/// channel can be routed to the right accumulator. Per-request
/// context (labels, prefix positions, new names, request-time URIs)
/// lives on the [`GroupAccum`] for that request instead — pending
/// entries only need a discriminator.
#[derive(Debug, Clone, Copy)]
pub enum LspRequestKind {
    Jump,
    References,
    Rename,
    CodeAction,
    CodeActionResolve,
    Hover,
    Completion,
    CompletionResolve,
    SignatureHelp,
}

/// What [`LspCoordinator::handle_event`] wants the caller to do.
/// Diagnostics events are absorbed internally; everything that requires
/// UI action surfaces here.
pub enum LspEventOutcome {
    /// No user-visible side effect required.
    Nothing,
    InfoMessage(String),
    ErrorMessage(String),
    Jump {
        label: &'static str,
        locations: Vec<Location>,
    },
    References(Vec<Location>),
    Rename {
        new_name: String,
        edit: Option<WorkspaceEdit>,
    },
    CodeActions(Vec<CodeAction>),
    CodeActionResolved(Option<CodeAction>),
    Hover(Option<Hover>),
    Completion {
        prefix_start: Cursor,
        items: Vec<CompletionItem>,
    },
    CompletionResolved {
        uri: String,
        /// `Some(idx)` when the resolve was fired from the open popup to
        /// fill in detail/documentation for the item at that index in
        /// `CompletionState.items`. `None` when the resolve was fired
        /// from `accept_completion` to pull `additionalTextEdits` (auto-
        /// imports) after the user already committed to an item.
        item_index: Option<usize>,
        /// The full resolved item (or `None` when the server returned
        /// something we couldn't parse). The handler picks `detail` /
        /// `documentation` off this for popup display, and
        /// `additional_text_edits` for the accept-time path.
        item: Option<CompletionItem>,
    },
    SignatureHelp {
        /// Row the request was made on. The handler closes the popup
        /// when the cursor has crossed to a different row in the
        /// meantime (stale response).
        anchor_row: usize,
        /// `None` when the server said we're no longer in a callable
        /// context — the handler treats this as "close any open popup".
        help: Option<SignatureHelp>,
    },
}

/// Per-client snapshot for the `:lsp` status modal. Produced by
/// [`LspCoordinator::running_clients`]; the UI formats one row from
/// each entry.
pub struct RunningLspInfo {
    pub client_key: String,
    pub pid: u32,
    pub root_uri: String,
    pub language_id: String,
    /// How many URIs the client currently holds `didOpen`'d.
    pub open_count: usize,
}

/// Result of applying a [`WorkspaceEdit`]. Other-file edits are written
/// to disk by the coordinator; the active buffer's edits are returned
/// for the caller to apply through its own `Buffer` (with undo, version
/// bump, etc.).
pub struct WorkspaceEditResult {
    pub current_buffer_edits: Vec<TextEdit>,
    pub files_touched: usize,
    pub total_edits: usize,
}

/// Accumulator state for an in-flight fan-out. One `Group` is allocated
/// per user-initiated LSP request and lives until every client we
/// dispatched to has either responded, errored, or been declared dead.
struct Group {
    /// How many client responses (or terminal errors) are still
    /// outstanding before we surface the merged outcome.
    remaining: usize,
    accum: GroupAccum,
}

enum GroupAccum {
    Jump {
        label: &'static str,
        locations: Vec<Location>,
    },
    References(Vec<Location>),
    /// First non-empty edit wins. Rename across multiple servers in a
    /// single buffer is rare and trying to merge edit lists from two
    /// servers could double-apply.
    Rename {
        new_name: String,
        edit: Option<WorkspaceEdit>,
    },
    CodeAction(Vec<CodeAction>),
    /// Joined with blank lines on emit.
    Hover(Vec<String>),
    Completion {
        prefix_start: Cursor,
        items: Vec<CompletionItem>,
    },
    /// Resolve outcomes are inherently single-client; the group just
    /// carries the per-request context until the one response arrives.
    /// `item_index` distinguishes a popup-display resolve (with the
    /// item slot to update) from an accept-time resolve (`None` —
    /// pulls auto-import edits).
    CompletionResolve {
        uri: String,
        item_index: Option<usize>,
        item: Option<CompletionItem>,
    },
    CodeActionResolve {
        action: Option<CodeAction>,
    },
    /// Signature help is single-client; the group just carries the
    /// anchor row for stale-response detection and accumulates the one
    /// response.
    SignatureHelp {
        anchor_row: usize,
        help: Option<SignatureHelp>,
    },
}

/// Per-pending-request bookkeeping. Held under
/// `pending[(client_key, request_id)]`.
struct Pending {
    group: u64,
    kind: LspRequestKind,
}

pub struct LspCoordinator {
    /// Live LSP clients, keyed by `client_key` (see [`client_key`]).
    clients: HashMap<String, LspClient>,
    /// Every URI we've sent `textDocument/didOpen` for, mapped to the
    /// list of `client_key`s currently holding it open. Buffer switches
    /// no longer `didClose` — we keep the server's view of every
    /// visited buffer alive so workspace-wide diagnostics survive
    /// while the buffer sleeps in vorto's parked/sleeping pool. Only
    /// `:bd` (or the client dying) sends `didClose` and removes the
    /// entry here. Used by `did_open` to skip a no-op re-open and by
    /// the workspace diagnostics picker via `all_diagnostics`.
    open_uris: HashMap<String, Vec<String>>,
    /// Diagnostics keyed first by URI, then by the client that
    /// published them. Merged across clients on read so the UI sees
    /// every server's findings at once. `publishDiagnostics` is
    /// authoritative per `(client, uri)` — an empty `items` from one
    /// client only clears that client's slice.
    diagnostics: HashMap<String, HashMap<String, Vec<Diagnostic>>>,
    /// Outstanding LSP request bookkeeping. Keyed by `(client_key, id)`
    /// so a response on the shared event channel routes back to the
    /// right pending entry.
    pending: HashMap<(String, u64), Pending>,
    /// Fan-out accumulators keyed by a per-request group id.
    groups: HashMap<u64, Group>,
    next_group_id: u64,
    /// URI of the document currently considered "open".
    current_uri: Option<String>,
    /// Language name of the currently-open document. Mostly cosmetic —
    /// `current_clients` is what drives request dispatch.
    current_language: Option<String>,
    /// Client keys attached to the current document. All fan-out
    /// requests dispatch to every entry here.
    current_clients: Vec<String>,
    last_synced_version: u64,
    event_tx: Sender<AppEvent>,
    startup_cwd: PathBuf,
}

impl LspCoordinator {
    pub fn new(event_tx: Sender<AppEvent>, startup_cwd: PathBuf) -> Self {
        Self {
            clients: HashMap::new(),
            open_uris: HashMap::new(),
            diagnostics: HashMap::new(),
            pending: HashMap::new(),
            groups: HashMap::new(),
            next_group_id: 0,
            current_uri: None,
            current_language: None,
            current_clients: Vec::new(),
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

    /// True when at least one client is attached to the current
    /// document. Drives the "no LSP for this buffer" guard in the App.
    pub fn has_lsp(&self) -> bool {
        self.current_uri.is_some() && !self.current_clients.is_empty()
    }

    /// True when any client attached to the current document declared
    /// `c` as a completion trigger character. Used by insert mode to
    /// auto-fire `textDocument/completion` on language-specific
    /// punctuation (e.g. `:` for Rust paths, `<` for TypeScript JSX)
    /// without hardcoding it on our side.
    pub fn is_completion_trigger_char(&self, c: char) -> bool {
        let mut buf = [0u8; 4];
        let needle = c.encode_utf8(&mut buf);
        self.current_clients.iter().any(|key| {
            self.clients
                .get(key)
                .map(|client| {
                    client
                        .completion_trigger_characters()
                        .iter()
                        .any(|t| t == needle)
                })
                .unwrap_or(false)
        })
    }

    /// True when any client attached to the current document declared
    /// `c` as a signature-help trigger character (typically `(`). The
    /// insert layer uses this to fire `textDocument/signatureHelp` on
    /// the specific punctuation each server cares about.
    pub fn is_signature_help_trigger_char(&self, c: char) -> bool {
        let mut buf = [0u8; 4];
        let needle = c.encode_utf8(&mut buf);
        self.current_clients.iter().any(|key| {
            self.clients
                .get(key)
                .map(|client| {
                    client
                        .signature_help_trigger_characters()
                        .iter()
                        .any(|t| t == needle)
                })
                .unwrap_or(false)
        })
    }

    /// Every (uri, diagnostic) pair the coordinator currently holds,
    /// merged across clients and sorted within each URI. Used by the
    /// workspace diagnostics picker; cloned because storage is keyed
    /// per-client and the caller wants a flat owned list.
    ///
    /// URI ordering is alphabetical so the picker order is stable
    /// across runs (`HashMap` iteration would otherwise reshuffle
    /// every restart).
    pub fn all_diagnostics(&self) -> Vec<(String, Vec<Diagnostic>)> {
        let mut out: Vec<(String, Vec<Diagnostic>)> = self
            .diagnostics
            .iter()
            .filter_map(|(uri, per_client)| {
                let mut merged: Vec<Diagnostic> = per_client
                    .values()
                    .flat_map(|v| v.iter().cloned())
                    .collect();
                if merged.is_empty() {
                    return None;
                }
                merged.sort_by_key(|d| (d.range.start.line, d.range.start.character));
                Some((uri.clone(), merged))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Merged diagnostics across all clients for the current buffer's
    /// URI, if any. Cloned into an owned Vec because the underlying
    /// storage is per-client and we have to fold across that on read.
    pub fn current_diagnostics(&self) -> Option<Vec<Diagnostic>> {
        let uri = self.current_uri.as_ref()?;
        let per_client = self.diagnostics.get(uri)?;
        let mut out: Vec<Diagnostic> = per_client
            .values()
            .flat_map(|v| v.iter().cloned())
            .collect();
        if out.is_empty() {
            return None;
        }
        // Sort for a stable presentation regardless of which server
        // happened to publish first.
        out.sort_by_key(|d| (d.range.start.line, d.range.start.character));
        Some(out)
    }

    /// Drop the "current document" pointers without telling the
    /// servers anything. The URI stays in `open_uris` so the server's
    /// view of the buffer (and its diagnostics) survives the switch —
    /// `<space>D` then sees every visited file, not just the active
    /// one. Use [`Self::close_uri`] when the buffer is actually being
    /// destroyed (`:bd`).
    pub fn detach_current(&mut self) {
        self.current_uri = None;
        self.current_clients.clear();
        self.current_language = None;
    }

    /// Explicitly tear down a URI's didOpen state across every client
    /// that has it open. Sends `didClose`, removes the URI from
    /// `open_uris`, and drops its diagnostics. Called by `:bd` so the
    /// server can release its copy when the buffer is gone for good.
    ///
    /// Once the `didClose` is sent, any client that no longer holds
    /// *any* URI open is shut down — `:bd`-ing the last Rust file
    /// reaps rust-analyzer instead of leaving it resident for the rest
    /// of the session. Sleeping/parked buffers still count as "open"
    /// here (their URIs stay in `open_uris` across buffer switches by
    /// design — see [`Self::detach_current`]), so a client only dies
    /// when every visited buffer for it has been explicitly deleted.
    pub fn close_uri(&mut self, uri: &str) {
        let client_keys = self.open_uris.remove(uri).unwrap_or_default();
        for key in &client_keys {
            if let Some(client) = self.clients.get_mut(key) {
                let _ = client.did_close(uri);
            }
        }
        self.diagnostics.remove(uri);
        if self.current_uri.as_deref() == Some(uri) {
            self.detach_current();
        }
        for key in &client_keys {
            let still_holding = self
                .open_uris
                .values()
                .any(|keys| keys.iter().any(|k| k == key));
            if !still_holding {
                self.drop_client(key);
            }
        }
    }

    /// Returns `true` when a client for `client_key` is already attached.
    pub fn has_client(&self, client_key: &str) -> bool {
        self.clients.contains_key(client_key)
    }

    /// Snapshot of every currently-running client for the `:lsp`
    /// status modal. Order is unspecified — the caller sorts.
    pub fn running_clients(&self) -> Vec<RunningLspInfo> {
        self.clients
            .iter()
            .map(|(key, client)| {
                let open_count = self
                    .open_uris
                    .values()
                    .filter(|keys| keys.iter().any(|k| k == key))
                    .count();
                RunningLspInfo {
                    client_key: key.clone(),
                    pid: client.pid(),
                    root_uri: client.root_uri().to_string(),
                    language_id: client.language_id().to_string(),
                    open_count,
                }
            })
            .collect()
    }

    /// Adopt a pre-spawned `LspClient`. Used by the file-open worker
    /// thread. Returns false (and the freshly-spawned client is dropped
    /// by the caller) when the same `client_key` is already attached —
    /// e.g. a parallel open of another file with the same language
    /// won the race.
    pub fn attach_client(&mut self, client_key: &str, client: LspClient) -> bool {
        if self.clients.contains_key(client_key) {
            return false;
        }
        self.clients.insert(client_key.to_string(), client);
        true
    }

    /// Mark `client_key` as one of the active clients for the current
    /// document. Idempotent. The caller (worker) is also responsible
    /// for firing `didOpen` against the new client.
    pub fn add_current_client(&mut self, client_key: &str) {
        if !self.current_clients.iter().any(|k| k == client_key) {
            self.current_clients.push(client_key.to_string());
        }
    }

    /// Build the `emit` closure passed to `LspClient::spawn`.
    pub fn make_emit(&self) -> Box<dyn Fn(LspEvent) + Send + 'static> {
        let tx = self.event_tx.clone();
        Box::new(move |ev| {
            let _ = tx.send(AppEvent::Lsp(ev));
        })
    }

    pub fn startup_cwd(&self) -> &Path {
        &self.startup_cwd
    }

    /// Send `didOpen` for `path` against `client_key` and mark it as
    /// the current document. When `client_key` already holds this URI
    /// open (the user is switching back to an already-visited buffer)
    /// the LSP notification is skipped — re-sending `didOpen` for an
    /// already-open document is forbidden by the spec and would make
    /// servers like tsserver reject the request. Either way the
    /// "current document" pointers are repointed so subsequent
    /// `did_change` / requests target this URI.
    pub fn did_open(
        &mut self,
        client_key: &str,
        lang_name: &str,
        path: &Path,
        text: &str,
    ) -> Result<()> {
        let uri = lsp::path_to_uri(path);
        let already_open = self
            .open_uris
            .get(&uri)
            .is_some_and(|keys| keys.iter().any(|k| k == client_key));
        if !already_open && let Some(client) = self.clients.get_mut(client_key) {
            client.did_open(&uri, text)?;
            self.open_uris
                .entry(uri.clone())
                .or_default()
                .push(client_key.to_string());
        }
        self.current_uri = Some(uri);
        self.current_language = Some(lang_name.to_string());
        self.add_current_client(client_key);
        Ok(())
    }

    /// Fan out `didChange` to every client attached to the current
    /// document. No-op when nothing is attached.
    pub fn did_change(&mut self, text: &str) -> Result<()> {
        let Some(uri) = self.current_uri.clone() else {
            return Ok(());
        };
        let keys = self.current_clients.clone();
        for key in &keys {
            if let Some(client) = self.clients.get_mut(key) {
                client.did_change(&uri, text)?;
            }
        }
        Ok(())
    }

    /// Synchronously request `textDocument/formatting` from the first
    /// attached client. Returns the parsed edits on success, `Ok(None)`
    /// when no client is attached (so the caller can fall through to
    /// an external formatter / no-op without inventing a sentinel
    /// error). `timeout` caps how long save will block before giving
    /// up — past that, the save proceeds un-formatted.
    ///
    /// We try only the first client rather than fanning out: format
    /// responses from two servers would need a merge strategy we
    /// haven't designed (and `vtsls` + `typescript-language-server`
    /// formatting the same file twice would just clobber each other).
    pub fn format_first_client(
        &mut self,
        options: Value,
        timeout: Duration,
    ) -> Result<Option<Vec<lsp::TextEdit>>> {
        let Some(uri) = self.current_uri.clone() else {
            return Ok(None);
        };
        let Some(key) = self.current_clients.first().cloned() else {
            return Ok(None);
        };
        let Some(client) = self.clients.get_mut(&key) else {
            return Ok(None);
        };
        let edits = client.formatting(&uri, options, timeout)?;
        Ok(Some(edits))
    }

    /// Fan out `didSave` to every client attached to the current
    /// document.
    pub fn did_save(&mut self, text: &str) -> Result<()> {
        let Some(uri) = self.current_uri.clone() else {
            return Ok(());
        };
        let keys = self.current_clients.clone();
        for key in &keys {
            if let Some(client) = self.clients.get_mut(key) {
                client.did_save(&uri, text)?;
            }
        }
        Ok(())
    }

    pub fn request_jump(
        &mut self,
        method: &str,
        label: &'static str,
        cursor: Cursor,
    ) -> Result<()> {
        let params = self.text_document_position_params(cursor);
        self.fan_out_request(
            method,
            params,
            LspRequestKind::Jump,
            GroupAccum::Jump {
                label,
                locations: Vec::new(),
            },
        )
    }

    pub fn request_references(&mut self, cursor: Cursor) -> Result<()> {
        let mut params = self.text_document_position_params(cursor);
        if let Some(obj) = params.as_object_mut() {
            obj.insert(
                "context".to_string(),
                serde_json::json!({ "includeDeclaration": true }),
            );
        }
        self.fan_out_request(
            "textDocument/references",
            params,
            LspRequestKind::References,
            GroupAccum::References(Vec::new()),
        )
    }

    pub fn request_code_action(
        &mut self,
        cursor: Cursor,
        diagnostics: &[Diagnostic],
    ) -> Result<()> {
        let uri = self.current_uri.clone().unwrap_or_default();
        let line = cursor.row as u64;
        let character = cursor.col as u64;
        let diagnostics_json = Value::Array(
            diagnostics
                .iter()
                .filter(|d| {
                    d.range.start.line <= cursor.row as u32 && cursor.row as u32 <= d.range.end.line
                })
                .map(diagnostic_to_json)
                .collect(),
        );
        let params = serde_json::json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": line, "character": character },
                "end":   { "line": line, "character": character },
            },
            "context": { "diagnostics": diagnostics_json },
        });
        self.fan_out_request(
            "textDocument/codeAction",
            params,
            LspRequestKind::CodeAction,
            GroupAccum::CodeAction(Vec::new()),
        )
    }

    pub fn request_hover(&mut self, cursor: Cursor) -> Result<()> {
        let params = self.text_document_position_params(cursor);
        self.fan_out_request(
            "textDocument/hover",
            params,
            LspRequestKind::Hover,
            GroupAccum::Hover(Vec::new()),
        )
    }

    /// `trigger` is `Some(c)` when the request was fired because the
    /// user typed `c` and the server declared it as a trigger character
    /// (`completionProvider.triggerCharacters`). `None` covers manual
    /// `<C-Space>` invocations and auto-fires on identifier chars. The
    /// distinction matters: rust-analyzer's path completion (`foo::|`)
    /// expects `triggerKind: 2 (TriggerCharacter)` with `triggerCharacter`
    /// set, and otherwise treats the request as a plain `Invoked`.
    pub fn request_completion(
        &mut self,
        cursor: Cursor,
        prefix_start: Cursor,
        trigger: Option<char>,
    ) -> Result<()> {
        let mut params = self.text_document_position_params(cursor);
        let context = match trigger {
            Some(c) => serde_json::json!({
                "triggerKind": 2,
                "triggerCharacter": c.to_string(),
            }),
            None => serde_json::json!({ "triggerKind": 1 }),
        };
        if let Some(obj) = params.as_object_mut() {
            obj.insert("context".to_string(), context);
        }
        self.fan_out_request(
            "textDocument/completion",
            params,
            LspRequestKind::Completion,
            GroupAccum::Completion {
                prefix_start,
                items: Vec::new(),
            },
        )
    }

    /// `textDocument/signatureHelp` — fans out to every attached client
    /// and the first non-null response wins. `trigger` maps onto LSP's
    /// `SignatureHelpContext`:
    /// - `Invoked` (programmatic, e.g. after accept-completion's
    ///   auto-`()`) sends `triggerKind: 1`.
    /// - `TriggerCharacter(c)` sends `triggerKind: 2` plus the actual
    ///   character — servers branch on this (e.g. `(` is "open from
    ///   scratch" vs `,` would arrive as `ContentChange` retrigger).
    /// - `ContentChange(c)` sends `triggerKind: 3` with `isRetrigger:
    ///   true` and the typed char when known. Used for the per-keystroke
    ///   refresh that keeps `activeParameter` aligned with the cursor.
    ///
    /// `active_help` is the currently-displayed help (when the popup is
    /// open) — passed back as `activeSignatureHelp` so the server can
    /// reconcile its view with what we're showing.
    pub fn request_signature_help(
        &mut self,
        cursor: Cursor,
        trigger: SignatureTrigger,
        active_help: Option<&SignatureHelp>,
    ) -> Result<()> {
        let mut params = self.text_document_position_params(cursor);
        let is_retrigger = matches!(trigger, SignatureTrigger::ContentChange(_));
        let mut context = match trigger {
            SignatureTrigger::Invoked => serde_json::json!({
                "triggerKind": 1,
                "isRetrigger": is_retrigger,
            }),
            SignatureTrigger::TriggerCharacter(c) => serde_json::json!({
                "triggerKind": 2,
                "triggerCharacter": c.to_string(),
                "isRetrigger": is_retrigger,
            }),
            SignatureTrigger::ContentChange(c) => {
                let mut o = serde_json::json!({
                    "triggerKind": 3,
                    "isRetrigger": true,
                });
                if let Some(c) = c {
                    o["triggerCharacter"] = Value::String(c.to_string());
                }
                o
            }
        };
        if let Some(help) = active_help {
            context["activeSignatureHelp"] = signature_help_to_json(help);
        }
        if let Some(obj) = params.as_object_mut() {
            obj.insert("context".to_string(), context);
        }
        self.fan_out_request(
            "textDocument/signatureHelp",
            params,
            LspRequestKind::SignatureHelp,
            GroupAccum::SignatureHelp {
                anchor_row: cursor.row,
                help: None,
            },
        )
    }

    /// `completionItem/resolve` — single-client. `source` is the
    /// `client_key` that originally produced the item; resolving via a
    /// different server would lose the opaque `data` context.
    ///
    /// `item_index` tags the call site: `Some(idx)` for popup-display
    /// resolves (the handler updates `CompletionState.items[idx]` with
    /// the returned detail / documentation); `None` for accept-time
    /// resolves (the handler applies the returned `additionalTextEdits`
    /// to the buffer).
    pub fn request_completion_resolve(
        &mut self,
        raw: Value,
        source: &str,
        item_index: Option<usize>,
    ) -> Result<()> {
        let uri = self.current_uri.clone().unwrap_or_default();
        self.send_single(
            source,
            "completionItem/resolve",
            raw,
            LspRequestKind::CompletionResolve,
            GroupAccum::CompletionResolve {
                uri,
                item_index,
                item: None,
            },
        )
    }

    pub fn current_uri(&self) -> Option<&str> {
        self.current_uri.as_deref()
    }

    /// `codeAction/resolve` — single-client. `source` is the `client_key`
    /// that originally produced the action.
    pub fn request_code_action_resolve(&mut self, action: Value, source: &str) -> Result<()> {
        self.send_single(
            source,
            "codeAction/resolve",
            action,
            LspRequestKind::CodeActionResolve,
            GroupAccum::CodeActionResolve { action: None },
        )
    }

    pub fn request_rename(&mut self, new_name: String, cursor: Cursor) -> Result<()> {
        let mut params = self.text_document_position_params(cursor);
        if let Some(obj) = params.as_object_mut() {
            obj.insert("newName".to_string(), Value::String(new_name.clone()));
        }
        let kind_new_name = new_name.clone();
        self.fan_out_request(
            "textDocument/rename",
            params,
            LspRequestKind::Rename,
            GroupAccum::Rename {
                new_name: kind_new_name,
                edit: None,
            },
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

    /// Allocate a group, dispatch `params` as `method` to every current
    /// client, register a pending entry for each, and stash the group's
    /// accumulator. When every client has either responded or had its
    /// `Pending` cleared on error, the accumulated state is surfaced as
    /// an [`LspEventOutcome`].
    fn fan_out_request(
        &mut self,
        method: &str,
        params: Value,
        kind: LspRequestKind,
        accum: GroupAccum,
    ) -> Result<()> {
        let keys = self.current_clients.clone();
        if keys.is_empty() {
            return Ok(());
        }
        let group_id = self.alloc_group();
        let mut sent = 0usize;
        for key in &keys {
            if let Some(client) = self.clients.get_mut(key) {
                match client.request(method, params.clone()) {
                    Ok(id) => {
                        self.pending.insert(
                            (key.clone(), id),
                            Pending {
                                group: group_id,
                                kind,
                            },
                        );
                        sent += 1;
                    }
                    Err(_) => {
                        // The reader thread will surface the underlying
                        // error separately; here we just don't count
                        // this client toward the group.
                    }
                }
            }
        }
        if sent == 0 {
            return Ok(());
        }
        self.groups.insert(
            group_id,
            Group {
                remaining: sent,
                accum,
            },
        );
        Ok(())
    }

    /// Single-client dispatch (used for resolve round-trips). Falls
    /// back to the first attached client when `source` is unknown — a
    /// stale completion whose originating server was disabled between
    /// the popup opening and the user pressing accept.
    fn send_single(
        &mut self,
        source: &str,
        method: &str,
        params: Value,
        kind: LspRequestKind,
        accum: GroupAccum,
    ) -> Result<()> {
        let key = if self.clients.contains_key(source) {
            source.to_string()
        } else if let Some(first) = self.current_clients.first().cloned() {
            first
        } else {
            return Ok(());
        };
        let Some(client) = self.clients.get_mut(&key) else {
            return Ok(());
        };
        let id = client.request(method, params)?;
        let group_id = self.alloc_group();
        self.pending.insert(
            (key, id),
            Pending {
                group: group_id,
                kind,
            },
        );
        self.groups.insert(
            group_id,
            Group {
                remaining: 1,
                accum,
            },
        );
        Ok(())
    }

    fn alloc_group(&mut self) -> u64 {
        let id = self.next_group_id;
        self.next_group_id = self.next_group_id.wrapping_add(1);
        id
    }

    /// Consume an LSP event. Diagnostics / messages are absorbed here;
    /// responses fold into their request group and an outcome is
    /// surfaced once every fanned-out client has reported back.
    pub fn handle_event(&mut self, ev: LspEvent) -> LspEventOutcome {
        match ev {
            LspEvent::Diagnostics { client, uri, items } => {
                let entry = self.diagnostics.entry(uri.clone()).or_default();
                if items.is_empty() {
                    entry.remove(&client);
                    if entry.is_empty() {
                        self.diagnostics.remove(&uri);
                    }
                } else {
                    entry.insert(client, items);
                }
                LspEventOutcome::Nothing
            }
            LspEvent::Message { level, text } => {
                if level == 1 {
                    LspEventOutcome::ErrorMessage(text)
                } else {
                    LspEventOutcome::InfoMessage(text)
                }
            }
            LspEvent::Error { client, message } => {
                // Reader-thread EOF after we proactively closed the
                // last URI for this client (see [`Self::close_uri`])
                // is expected, not an error. The client is already
                // gone from `self.clients` at that point — skip both
                // the redundant drop and the toast.
                if !self.clients.contains_key(&client) {
                    return LspEventOutcome::Nothing;
                }
                self.drop_client(&client);
                LspEventOutcome::ErrorMessage(format!("lsp: {}", message))
            }
            LspEvent::Response {
                client,
                id,
                result,
                error,
            } => self.handle_response(client, id, result, error),
        }
    }

    /// Route a `Response` back to its `Group` and, if every client in
    /// that group has now reported, emit the merged outcome.
    fn handle_response(
        &mut self,
        client: String,
        id: u64,
        result: Option<Value>,
        error: Option<String>,
    ) -> LspEventOutcome {
        let Some(pending) = self.pending.remove(&(client.clone(), id)) else {
            return LspEventOutcome::Nothing;
        };
        let group_id = pending.group;
        let result = result.unwrap_or(Value::Null);

        // Errors collapse to "empty result" — we still count this
        // client toward the group so the merge eventually completes,
        // but the user shouldn't see a per-server error for each
        // server in the fan-out. Genuine failures (every server
        // errored) leave the merged outcome empty, which downstream
        // handlers already report as "no results".
        let had_error = error.is_some();

        if let Some(group) = self.groups.get_mut(&group_id) {
            if !had_error {
                accumulate(&mut group.accum, &client, &result, &pending.kind);
            }
            group.remaining = group.remaining.saturating_sub(1);
            if group.remaining == 0 {
                let group = self.groups.remove(&group_id).unwrap();
                return finalize(group.accum);
            }
        }
        LspEventOutcome::Nothing
    }

    /// Drop a client whose reader thread is dead, or which no URI
    /// references anymore (see [`Self::close_uri`]). Pending requests
    /// against it count as already-responded (empty); groups that
    /// finalise as a result of this are surfaced via the event channel
    /// so the caller sees the merged outcome without a follow-up
    /// response event.
    fn drop_client(&mut self, client_key: &str) {
        let removed = self.clients.remove(client_key);
        self.current_clients.retain(|k| k != client_key);
        // Decrement remaining counts for every pending request against
        // this client. Groups that hit zero are dropped on the floor —
        // the user already sees an `ErrorMessage` for the reader-thread
        // failure that triggered us, which is informative enough; the
        // alternative (a second outcome for the merged-but-incomplete
        // result) would need a new AppEvent variant.
        let dead_keys: Vec<(String, u64)> = self
            .pending
            .keys()
            .filter(|(k, _)| k == client_key)
            .cloned()
            .collect();
        for k in dead_keys {
            if let Some(pending) = self.pending.remove(&k)
                && let Some(group) = self.groups.get_mut(&pending.group)
            {
                group.remaining = group.remaining.saturating_sub(1);
                if group.remaining == 0 {
                    self.groups.remove(&pending.group);
                }
            }
        }
        // Drop diagnostics this client owned across all URIs so the
        // status bar doesn't show stale entries from a dead server.
        for slices in self.diagnostics.values_mut() {
            slices.remove(client_key);
        }
        self.diagnostics.retain(|_, slices| !slices.is_empty());
        // The dead client no longer holds anything open — drop its
        // entries so a future `did_open` for a previously-visited URI
        // re-fires (against whichever client picks the language up
        // next) instead of being skipped as a duplicate.
        for keys in self.open_uris.values_mut() {
            keys.retain(|k| k != client_key);
        }
        self.open_uris.retain(|_, keys| !keys.is_empty());
        // Dispose off-thread: `LspClient::Drop` does the
        // `shutdown`/`exit` handshake and reaps the child, which can
        // block ~800ms for servers like rust-analyzer. Running it
        // inline would stall `:bd` of the last buffer for a language.
        if let Some(client) = removed {
            thread::spawn(move || drop(client));
        }
    }

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

/// Fold a single client's response into the group's accumulator.
fn accumulate(accum: &mut GroupAccum, source: &str, result: &Value, kind: &LspRequestKind) {
    match (accum, kind) {
        (GroupAccum::Jump { locations, .. }, LspRequestKind::Jump) => {
            locations.extend(lsp::parse_locations(result));
        }
        (GroupAccum::References(locations), LspRequestKind::References) => {
            locations.extend(lsp::parse_locations(result));
        }
        (GroupAccum::Rename { edit, .. }, LspRequestKind::Rename) if edit.is_none() => {
            *edit = lsp::parse_workspace_edit(result);
        }
        (GroupAccum::CodeAction(actions), LspRequestKind::CodeAction) => {
            let mut parsed = lsp::parse_code_actions(result);
            for a in &mut parsed {
                a.source = source.to_string();
            }
            actions.extend(parsed);
        }
        (GroupAccum::Hover(parts), LspRequestKind::Hover) => {
            if let Some(h) = lsp::parse_hover(result) {
                parts.push(h.contents);
            }
        }
        (GroupAccum::Completion { items, .. }, LspRequestKind::Completion) => {
            let mut parsed = lsp::parse_completion(result);
            for it in &mut parsed {
                it.source = source.to_string();
            }
            items.extend(parsed);
        }
        (GroupAccum::CompletionResolve { item, .. }, LspRequestKind::CompletionResolve) => {
            // Servers that don't support resolve typically echo the
            // item back unchanged (or return null); both shapes parse
            // to either `None` or an item with no new fields, which
            // the handler treats as a no-op.
            *item = lsp::parse_completion_resolve(result);
            if let Some(it) = item.as_mut() {
                it.source = source.to_string();
            }
        }
        (GroupAccum::CodeActionResolve { action }, LspRequestKind::CodeActionResolve) => {
            let mut parsed = lsp::parse_code_action(result);
            if let Some(a) = parsed.as_mut() {
                a.source = source.to_string();
            }
            *action = parsed;
        }
        (GroupAccum::SignatureHelp { help, .. }, LspRequestKind::SignatureHelp)
            if help.is_none() =>
        {
            // First non-null response wins — fanning out to two servers
            // would otherwise need a merge strategy we don't have, and
            // signature help is inherently "one signature at a time".
            *help = lsp::parse_signature_help(result);
        }
        _ => {}
    }
}

/// Emit the merged outcome once every fanned-out client has reported.
fn finalize(accum: GroupAccum) -> LspEventOutcome {
    match accum {
        GroupAccum::Jump { label, locations } => LspEventOutcome::Jump { label, locations },
        GroupAccum::References(locations) => LspEventOutcome::References(locations),
        GroupAccum::Rename { new_name, edit } => LspEventOutcome::Rename { new_name, edit },
        GroupAccum::CodeAction(actions) => LspEventOutcome::CodeActions(actions),
        GroupAccum::Hover(parts) => {
            if parts.is_empty() {
                LspEventOutcome::Hover(None)
            } else {
                LspEventOutcome::Hover(Some(Hover {
                    contents: parts.join("\n\n---\n\n"),
                }))
            }
        }
        GroupAccum::Completion {
            prefix_start,
            items,
        } => {
            let items = dedup_completion(items);
            LspEventOutcome::Completion {
                prefix_start,
                items,
            }
        }
        GroupAccum::CompletionResolve {
            uri,
            item_index,
            item,
        } => LspEventOutcome::CompletionResolved {
            uri,
            item_index,
            item,
        },
        GroupAccum::CodeActionResolve { action } => LspEventOutcome::CodeActionResolved(action),
        GroupAccum::SignatureHelp { anchor_row, help } => {
            LspEventOutcome::SignatureHelp { anchor_row, help }
        }
    }
}

/// Round-trip our `SignatureHelp` back into the LSP wire shape so we
/// can echo it in `activeSignatureHelp` on retriggers. The server uses
/// this to reconcile its view against the popup the user is currently
/// looking at — without it, retrigger context is missing the "what
/// were we showing" half.
///
/// Documentation and parameter labels round-trip as plain text/offsets;
/// any per-parameter docs are dropped (servers don't need them back).
fn signature_help_to_json(help: &SignatureHelp) -> Value {
    let signatures: Vec<Value> = help
        .signatures
        .iter()
        .map(|s| {
            let parameters: Vec<Value> = s
                .parameters
                .iter()
                .map(|p| match &p.label {
                    lsp::ParameterLabel::Text(t) => serde_json::json!({ "label": t }),
                    lsp::ParameterLabel::Offsets(start, end) => {
                        serde_json::json!({ "label": [start, end] })
                    }
                })
                .collect();
            let mut obj = serde_json::json!({
                "label": s.label,
                "parameters": parameters,
            });
            if let Some(ap) = s.active_parameter {
                obj["activeParameter"] = Value::from(ap);
            }
            obj
        })
        .collect();
    let mut obj = serde_json::json!({
        "signatures": signatures,
        "activeSignature": help.active_signature,
    });
    obj["activeParameter"] = match help.active_parameter {
        Some(n) => Value::from(n),
        None => Value::Null,
    };
    obj
}

/// Strip duplicate completion items that bubbled up from multiple
/// servers offering the same symbol. Keys on `(label, kind,
/// insert_text-or-newText)` so legitimately-distinct items (same name,
/// different signatures) survive.
fn dedup_completion(items: Vec<CompletionItem>) -> Vec<CompletionItem> {
    use std::collections::HashSet;
    let mut seen: HashSet<(String, u8, String)> = HashSet::new();
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        let text_key = it
            .text_edit
            .as_ref()
            .map(|te| te.new_text.clone())
            .or_else(|| it.insert_text.clone())
            .unwrap_or_else(|| it.label.clone());
        let key = (it.label.clone(), it.kind, text_key);
        if seen.insert(key) {
            out.push(it);
        }
    }
    out
}

fn diagnostic_to_json(d: &Diagnostic) -> Value {
    serde_json::json!({
        "range": {
            "start": { "line": d.range.start.line, "character": d.range.start.character },
            "end":   { "line": d.range.end.line,   "character": d.range.end.character },
        },
        "severity": d.severity as u8 + 1,
        "message": d.message,
        "source": d.source,
    })
}
