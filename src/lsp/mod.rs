//! Minimal LSP client.
//!
//! One [`LspClient`] per language, spawned lazily the first time a buffer
//! of that language is opened. The client owns the server subprocess, a
//! reader thread that parses incoming JSON-RPC messages, and bookkeeping
//! for tracked documents.
//!
//! Threading model:
//! - The main thread writes requests/notifications to the server's stdin
//!   synchronously (it's a buffered pipe — writes are cheap).
//! - A per-client reader thread blocks on stdout, parses framed messages,
//!   and forwards interesting ones to the App via an mpsc channel.
//!
//! Implemented: `initialize` handshake, full-document sync
//! (`didOpen`/`didChange`/`didSave`/`didClose`), `publishDiagnostics`,
//! goto-definition / declaration / implementation, references, rename,
//! code actions (+ `codeAction/resolve`), and hover. Completion,
//! signature help, and inlay hints are intentionally out of scope.
//!
//! Submodules:
//! - [`types`] — normalised wire-protocol structs/enums.
//! - [`codec`] — JSON-RPC framing + the per-client reader thread.
//! - [`parse`] — pure parsers from `serde_json::Value` into [`types`].
//! - [`uri`] — `file://` ↔ `Path` conversion.
//! - [`root`] — workspace-root discovery.
//! - [`edits`] — applying [`TextEdit`]s to an in-memory line buffer.

use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::config::LspConfig;

mod codec;
mod edits;
mod parse;
mod root;
mod types;
mod uri;

pub use edits::apply_text_edits;
pub use parse::{
    parse_code_action, parse_code_actions, parse_completion, parse_hover, parse_locations,
    parse_workspace_edit,
};
pub use root::discover_root;
pub use types::{
    CodeAction, CompletionItem, Diagnostic, Hover, Location, LspEvent, Severity, TextEdit,
    WorkspaceEdit,
};
pub use uri::{path_to_uri, uri_to_path};

use codec::{notification, read_message, reader_loop, request, write_framed};

pub struct LspClient {
    /// Kept alive so the child isn't reaped while we hold its pipes.
    _child: Child,
    /// Shared with the reader thread so it can reply to server-to-client
    /// requests (`client/registerCapability`, `workspace/configuration`,
    /// `window/workDoneProgress/create`, …) without round-tripping
    /// through the App.
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: u64,
    /// Documents we've sent `didOpen` for — `uri → version`. The version
    /// is bumped on every `didChange` so we don't need to track it on
    /// the Buffer side.
    docs: HashMap<String, i32>,
    /// `languageId` to send in `didOpen`.
    language_id: String,
}

impl LspClient {
    /// Spawn the server, run the initialize handshake synchronously, then
    /// detach a reader thread that forwards future messages to `tx`.
    pub fn spawn(
        lang_name: &str,
        spec: &LspConfig,
        root_uri: &str,
        emit: Box<dyn Fn(LspEvent) + Send + 'static>,
    ) -> Result<Self> {
        let mut child = Command::new(&spec.command)
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning LSP server `{}`", spec.command))?;

        let stdin_raw = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let mut reader = BufReader::new(stdout);
        let stdin = Arc::new(Mutex::new(stdin_raw));

        let workspace_name = root_uri
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or("workspace")
            .to_string();
        let init_id: u64 = 1;
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": workspace_name }],
            "capabilities": {
                "workspace": {
                    "configuration": false,
                    "workspaceFolders": true,
                    "didChangeConfiguration": { "dynamicRegistration": false },
                    "didChangeWatchedFiles": { "dynamicRegistration": true }
                },
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "didSave": true
                    },
                    "publishDiagnostics": { "relatedInformation": false },
                    // rust-analyzer (and others) gate refactor assists on
                    // `codeActionLiteralSupport`; without it the server only
                    // returns plain `Command`s, which we don't execute, so
                    // the picker would always look empty. `resolveSupport`
                    // tells the server it's allowed to defer the heavy
                    // `edit` until `codeAction/resolve`.
                    "codeAction": {
                        "dynamicRegistration": false,
                        "codeActionLiteralSupport": {
                            "codeActionKind": {
                                "valueSet": [
                                    "", "quickfix", "refactor",
                                    "refactor.extract", "refactor.inline",
                                    "refactor.rewrite", "source",
                                    "source.organizeImports"
                                ]
                            }
                        },
                        "resolveSupport": { "properties": ["edit"] },
                        "dataSupport": true
                    },
                    // We never request snippet expansion — the popup
                    // inserts `newText` verbatim, so `$0` / `${1:x}`
                    // tokens would land in the buffer as literal text.
                    // Declaring `snippetSupport: false` keeps servers
                    // honest (rust-analyzer emits a different `newText`
                    // shape when snippets are off).
                    "completion": {
                        "dynamicRegistration": false,
                        "completionItem": {
                            "snippetSupport": false,
                            "insertReplaceSupport": true,
                            "labelDetailsSupport": true
                        }
                    }
                },
                "window": {
                    "workDoneProgress": true
                }
            },
            "clientInfo": { "name": "vorto" },
        });
        write_framed(&stdin, &request(init_id, "initialize", init_params))?;

        // Drain messages until we see the initialize response. The server
        // can interleave its own requests (workspace/configuration,
        // client/registerCapability, window/workDoneProgress/create)
        // before answering ours — we have to reply to those right here
        // or the handshake deadlocks.
        loop {
            let msg = read_message(&mut reader).with_context(|| "reading initialize response")?;
            let is_init_response = msg.get("id").and_then(|v| v.as_u64()) == Some(init_id)
                && msg.get("method").is_none();
            if is_init_response {
                if let Some(err) = msg.get("error") {
                    bail!("LSP initialize error: {}", err);
                }
                break;
            }
            codec::handle_server_request(&stdin, &msg);
        }

        write_framed(&stdin, &notification("initialized", json!({})))?;

        let language_id = spec
            .language_id
            .clone()
            .unwrap_or_else(|| lang_name.to_string());

        let stdin_reader = Arc::clone(&stdin);
        let lang_for_reader = lang_name.to_string();
        thread::spawn(move || reader_loop(reader, emit, stdin_reader, lang_for_reader));

        Ok(Self {
            _child: child,
            stdin,
            next_id: 2,
            docs: HashMap::new(),
            language_id,
        })
    }

    /// Send an arbitrary JSON-RPC request. Returns the assigned id so the
    /// caller can match it against the eventual [`LspEvent::Response`].
    pub fn request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        write_framed(&self.stdin, &request(id, method, params))?;
        Ok(id)
    }

    pub fn did_open(&mut self, uri: &str, text: &str) -> Result<()> {
        if self.docs.contains_key(uri) {
            return Ok(());
        }
        self.docs.insert(uri.to_string(), 1);
        let params = json!({
            "textDocument": {
                "uri": uri,
                "languageId": self.language_id,
                "version": 1,
                "text": text,
            }
        });
        write_framed(&self.stdin, &notification("textDocument/didOpen", params))
    }

    /// Full-document sync — the simplest correct option for MVP. We bump
    /// the per-doc version and send the whole text every time.
    pub fn did_change(&mut self, uri: &str, text: &str) -> Result<()> {
        let v = match self.docs.get_mut(uri) {
            Some(v) => {
                *v += 1;
                *v
            }
            None => return Ok(()),
        };
        let params = json!({
            "textDocument": { "uri": uri, "version": v },
            "contentChanges": [ { "text": text } ],
        });
        write_framed(&self.stdin, &notification("textDocument/didChange", params))
    }

    pub fn did_save(&mut self, uri: &str, text: &str) -> Result<()> {
        if !self.docs.contains_key(uri) {
            return Ok(());
        }
        let params = json!({
            "textDocument": { "uri": uri },
            "text": text,
        });
        write_framed(&self.stdin, &notification("textDocument/didSave", params))
    }

    pub fn did_close(&mut self, uri: &str) -> Result<()> {
        if self.docs.remove(uri).is_none() {
            return Ok(());
        }
        let params = json!({ "textDocument": { "uri": uri } });
        write_framed(&self.stdin, &notification("textDocument/didClose", params))
    }
}
