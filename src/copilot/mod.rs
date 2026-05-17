//! GitHub Copilot LSP client.
//!
//! `copilot-language-server` (the official Microsoft/GitHub-published
//! Node binary) speaks standard LSP over stdio, so the JSON-RPC framing
//! lives in [`crate::lsp::codec`]. This module owns the Copilot-specific
//! pieces: spawn + handshake with the `editorInfo` /
//! `editorPluginInfo` initialization options it expects, a single
//! workspace-wide instance (no per-language fan-out), and silent
//! degradation when the binary isn't on `PATH` — vorto stays usable
//! either way, the user just doesn't get ghost-text completions.
//!
//! Phase 1 ships only spawn + handshake + a reader thread that surfaces
//! server messages and fatal errors. Document sync, sign-in, and the
//! actual `textDocument/inlineCompletion` request/response land in
//! follow-up commits — keeping each layer small enough to verify in
//! isolation.

use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::lsp::codec::{
    handle_server_request, notification, read_message, request, write_framed,
};
use crate::vlog;

/// Name of the Copilot server binary. Looked up via `PATH`; not bundled
/// with vorto and not installed automatically — by design the editor
/// stays out of the user's npm install loop.
const COPILOT_BIN: &str = "copilot-language-server";

/// Wait budgets used by [`CopilotClient::drop`] for the LSP-spec
/// shutdown handshake. Same shape as the values used by [`crate::lsp`]
/// — `shutdown` reply first, then `exit` notification, then a brief
/// poll before SIGKILL.
const SHUTDOWN_REPLY_WAIT: Duration = Duration::from_millis(500);
const EXIT_DRAIN_WAIT: Duration = Duration::from_millis(300);

/// Events emitted by the reader thread for the main loop to consume.
/// Inline-completion responses and sign-in status updates will join
/// this enum as the corresponding request paths land — for Phase 1
/// only the cross-cutting message / error cases exist.
#[derive(Debug)]
pub enum CopilotEvent {
    /// `window/showMessage` or `window/logMessage` from the server.
    /// `level` follows the LSP severity numbering (1=error, 4=log).
    Message { level: u8, text: String },
    /// Reader thread exited (EOF, parse failure, …). The client is
    /// effectively dead from this point — the App should drop its
    /// handle so a future request triggers a re-spawn attempt.
    Error { message: String },
}

pub struct CopilotClient {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: u64,
}

impl CopilotClient {
    /// Spawn the Copilot LSP server, run the initialize handshake
    /// synchronously, then detach a reader thread that forwards future
    /// messages through `emit`.
    ///
    /// Returns `Ok(None)` (not `Err`) when the binary isn't on `PATH`:
    /// Copilot is optional, the editor should keep working without
    /// any visible complaint. Other spawn / handshake failures bubble
    /// up via `Err` for the caller to log.
    pub fn spawn(
        workspace_root_uri: &str,
        emit: Box<dyn Fn(CopilotEvent) + Send + 'static>,
    ) -> Result<Option<Self>> {
        let mut child = match Command::new(COPILOT_BIN)
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                vlog!("copilot spawn skipped: `{}` not on PATH", COPILOT_BIN);
                return Ok(None);
            }
            Err(e) => return Err(e).with_context(|| format!("spawning `{}`", COPILOT_BIN)),
        };

        let stdin_raw = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let mut reader = BufReader::new(stdout);
        let stdin = Arc::new(Mutex::new(stdin_raw));

        let init_id: u64 = 1;
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": workspace_root_uri,
            "capabilities": {
                "workspace": { "workspaceFolders": true },
                "textDocument": {
                    "synchronization": { "didSave": true },
                    // Declares we accept `textDocument/inlineCompletion`
                    // responses (LSP 3.18). Without this the server
                    // skips the request path entirely even when the
                    // request is sent.
                    "inlineCompletion": {}
                }
            },
            // Copilot rejects the handshake unless both editorInfo and
            // editorPluginInfo are present — distinct from other LSP
            // servers, which leave initializationOptions optional.
            "initializationOptions": {
                "editorInfo": { "name": "vorto", "version": env!("CARGO_PKG_VERSION") },
                "editorPluginInfo": { "name": "vorto", "version": env!("CARGO_PKG_VERSION") }
            },
            "clientInfo": { "name": "vorto", "version": env!("CARGO_PKG_VERSION") }
        });
        write_framed(&stdin, &request(init_id, "initialize", init_params))?;

        // Drain server-to-client requests interleaved with our init
        // response. Same pattern as the standard LSP client — Copilot
        // tends to fire `window/workDoneProgress/create` before
        // replying.
        loop {
            let msg = read_message(&mut reader).with_context(|| "reading initialize response")?;
            let is_init_response = msg.get("id").and_then(|v| v.as_u64()) == Some(init_id)
                && msg.get("method").is_none();
            if is_init_response {
                if let Some(err) = msg.get("error") {
                    bail!("copilot initialize error: {}", err);
                }
                break;
            }
            handle_server_request(&stdin, &msg);
        }

        write_framed(&stdin, &notification("initialized", json!({})))?;
        vlog!("copilot spawn ok pid={}", child.id());

        let stdin_reader = Arc::clone(&stdin);
        thread::spawn(move || reader_loop(reader, emit, stdin_reader));

        Ok(Some(Self {
            child,
            stdin,
            next_id: 2,
        }))
    }
}

impl Drop for CopilotClient {
    /// Mirrors [`crate::lsp::LspClient`]'s graceful-shutdown pattern:
    /// `shutdown` request, wait briefly, `exit` notification, then a
    /// short poll before SIGKILL. Without this the Node server lingers
    /// after the editor quits.
    fn drop(&mut self) {
        vlog!("copilot shutdown begin pid={}", self.child.id());
        let shutdown_id = self.next_id;
        self.next_id += 1;

        let shutdown_sent =
            write_framed(&self.stdin, &request(shutdown_id, "shutdown", Value::Null)).is_ok();
        if shutdown_sent {
            // No blocking-reply channel yet — give the server a fixed
            // window to acknowledge, then send `exit` regardless. The
            // reader thread will swallow the response asynchronously.
            thread::sleep(SHUTDOWN_REPLY_WAIT);
            let _ = write_framed(&self.stdin, &notification("exit", Value::Null));
        }
        let deadline = std::time::Instant::now() + EXIT_DRAIN_WAIT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    vlog!("copilot shutdown clean status={status}");
                    return;
                }
                Ok(None) if std::time::Instant::now() >= deadline => break,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        vlog!("copilot shutdown kill");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn reader_loop(
    mut reader: BufReader<std::process::ChildStdout>,
    emit: Box<dyn Fn(CopilotEvent) + Send>,
    stdin: Arc<Mutex<ChildStdin>>,
) {
    loop {
        let msg = match read_message(&mut reader) {
            Ok(m) => m,
            Err(e) => {
                vlog!("copilot reader exit err={:#}", e);
                emit(CopilotEvent::Error {
                    message: format!("copilot reader: {e}"),
                });
                return;
            }
        };
        // Server-to-client request: has both `id` and `method`. Reply
        // generically — sign-in / completion paths will eventually need
        // structured replies but Phase 1 only sees boilerplate setup
        // requests (workDoneProgress/create etc.).
        if msg.get("id").is_some() && msg.get("method").is_some() {
            handle_server_request(&stdin, &msg);
            continue;
        }
        // Responses to requests we sent: nothing to dispatch yet —
        // Phase 1 only sends `initialize` (consumed synchronously)
        // and `shutdown` (fire-and-forget from `Drop`).
        if msg.get("method").is_none() {
            continue;
        }
        let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
        match method {
            "window/showMessage" | "window/logMessage" => {
                if let Some(params) = msg.get("params") {
                    let level = params.get("type").and_then(|v| v.as_u64()).unwrap_or(3) as u8;
                    let text = params
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    vlog!("copilot {} level={} {}", method, level, text);
                    emit(CopilotEvent::Message { level, text });
                }
            }
            _ => {
                // $/progress and other notifications are ignored — the
                // editor has no UI for Copilot progress yet.
            }
        }
    }
}
