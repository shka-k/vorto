//! GitHub Copilot LSP client.
//!
//! `copilot-language-server` (the official Microsoft/GitHub-published
//! Node binary) speaks standard LSP over stdio, so the JSON-RPC framing
//! lives in [`crate::lsp::codec`]. This module owns the Copilot-specific
//! pieces: spawn + handshake with the `editorInfo` /
//! `editorPluginInfo` initialization options it expects, a single
//! workspace-wide instance (no per-language fan-out), document sync,
//! `textDocument/inlineCompletion` requests, and silent degradation
//! when the binary isn't on `PATH` — vorto stays usable either way,
//! the user just doesn't get ghost-text completions.
//!
//! Routing model: the reader thread is intentionally dumb. All
//! responses to client-initiated requests are forwarded as
//! [`CopilotEvent::Response`] with the raw `result` / `error` JSON;
//! the App layer matches the request id against its own pending-kind
//! map and parses accordingly. Keeps protocol-shape knowledge in one
//! place (App) and avoids leaking inline-completion / sign-in types
//! into the codec layer.

use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::lsp::codec::{handle_server_request, notification, read_message, request, write_framed};
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
/// Reader-side already logs server messages via [`vlog!`] before
/// emitting, so this enum only carries variants the App needs to act
/// on; informational `window/logMessage` traffic doesn't round-trip.
#[derive(Debug)]
pub enum CopilotEvent {
    /// Response to a client-initiated request. Routed by id at the App
    /// layer against its pending-kind map; the raw JSON is forwarded
    /// so this module doesn't need to know about every request shape.
    Response {
        id: u64,
        result: Option<Value>,
        error: Option<String>,
    },
    /// Reader thread exited (EOF, parse failure, …). The client is
    /// effectively dead from this point — the App should drop its
    /// handle so a future request triggers a re-spawn attempt.
    Error { message: String },
}

/// Per-document sync bookkeeping. `lsp_version` is the i32 the
/// server expects on `didChange`; `buffer_version` is the editor-side
/// `Buffer::version` snapshot we last pushed — lets the caller's
/// dirty-check stay a single field comparison without parallel
/// per-URI tracking on the App side.
#[derive(Debug, Clone, Copy)]
struct DocState {
    lsp_version: i32,
    buffer_version: u64,
}

pub struct CopilotClient {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: u64,
    /// URI → tracked document state. Entries cleared on
    /// [`Self::did_close`].
    docs: HashMap<String, DocState>,
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
            docs: HashMap::new(),
        }))
    }

    /// True when the server's view of `uri` is out of date with
    /// `current_buffer_version` (or the URI has never been opened).
    /// Lets the App's dirty-flush path stay a single check that
    /// handles both "first sight" and "subsequent edit" without
    /// parallel App-side per-URI tracking.
    pub fn needs_sync(&self, uri: &str, current_buffer_version: u64) -> bool {
        match self.docs.get(uri) {
            Some(state) => state.buffer_version != current_buffer_version,
            None => true,
        }
    }

    /// Whether `uri` has already received a `didOpen`. Lets the
    /// caller decide between `did_open` and `did_change` when both
    /// would be valid.
    pub fn is_open(&self, uri: &str) -> bool {
        self.docs.contains_key(uri)
    }

    /// Send `textDocument/didOpen` and start tracking the document.
    /// No-op when already open — re-opens are silently skipped so a
    /// buffer switch can call this unconditionally as part of the
    /// sync gate.
    pub fn did_open(
        &mut self,
        uri: &str,
        language_id: &str,
        text: &str,
        buffer_version: u64,
    ) -> Result<()> {
        if self.docs.contains_key(uri) {
            return Ok(());
        }
        self.docs.insert(
            uri.to_string(),
            DocState {
                lsp_version: 1,
                buffer_version,
            },
        );
        let params = json!({
            "textDocument": {
                "uri": uri,
                "languageId": language_id,
                "version": 1,
                "text": text,
            }
        });
        write_framed(&self.stdin, &notification("textDocument/didOpen", params))
    }

    /// Full-document sync. Bumps the per-doc LSP version and records
    /// the buffer-version watermark so a future [`Self::needs_sync`]
    /// short-circuits when nothing changed.
    pub fn did_change(&mut self, uri: &str, text: &str, buffer_version: u64) -> Result<()> {
        let lsp_version = match self.docs.get_mut(uri) {
            Some(state) => {
                state.lsp_version += 1;
                state.buffer_version = buffer_version;
                state.lsp_version
            }
            None => return Ok(()),
        };
        let params = json!({
            "textDocument": { "uri": uri, "version": lsp_version },
            "contentChanges": [ { "text": text } ],
        });
        write_framed(&self.stdin, &notification("textDocument/didChange", params))
    }

    /// Reserved for the upcoming buffer-close hook — currently never
    /// fires (Copilot leaks doc state for buffers we drop, which is
    /// harmless until a long session with many opens), but keeping the
    /// method here means the close path lands in one file when it's
    /// wired.
    #[allow(dead_code)]
    pub fn did_close(&mut self, uri: &str) -> Result<()> {
        if self.docs.remove(uri).is_none() {
            return Ok(());
        }
        let params = json!({ "textDocument": { "uri": uri } });
        write_framed(&self.stdin, &notification("textDocument/didClose", params))
    }

    /// Fire `checkStatus` to learn whether the server has a usable
    /// auth token. Returns the request id; the response parses
    /// through [`parse_check_status`].
    ///
    /// `local_checks_only` (`true`) avoids a network round-trip to
    /// GitHub — fine for the post-spawn "is the user signed in?"
    /// check. Set `false` to force a full token validation against
    /// the API.
    pub fn check_status(&mut self, local_checks_only: bool) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let params = json!({ "options": { "localChecksOnly": local_checks_only } });
        write_framed(&self.stdin, &request(id, "checkStatus", params))?;
        Ok(id)
    }

    /// Start a device-flow sign-in. Server replies with a userCode and
    /// verificationUri the user pastes into a browser. After showing
    /// those to the user, call [`Self::sign_in_confirm`] with the
    /// userCode — Copilot's confirm reply blocks server-side until the
    /// user authorizes (or it times out, typically ~15 min), so the
    /// response arrives asynchronously through the reader thread.
    pub fn sign_in_initiate(&mut self) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        write_framed(&self.stdin, &request(id, "signInInitiate", json!({})))?;
        Ok(id)
    }

    pub fn sign_in_confirm(&mut self, user_code: &str) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let params = json!({ "userCode": user_code });
        write_framed(&self.stdin, &request(id, "signInConfirm", params))?;
        Ok(id)
    }

    pub fn sign_out(&mut self) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        write_framed(&self.stdin, &request(id, "signOut", json!({})))?;
        Ok(id)
    }

    /// Fire `textDocument/inlineCompletion` for the given cursor
    /// position. Returns the request id so the caller can match the
    /// response against its pending-kind map.
    ///
    /// `line` and `character` are 0-based, per LSP. Triggered by
    /// `Invoked` (kind=1) for now — `Automatic` (kind=2) would be the
    /// signal when typed text triggered the request, but Copilot
    /// treats them identically in practice.
    pub fn inline_completion(&mut self, uri: &str, line: u32, character: u32) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "context": { "triggerKind": 1 }
        });
        write_framed(
            &self.stdin,
            &request(id, "textDocument/inlineCompletion", params),
        )?;
        Ok(id)
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
        let is_response = msg.get("method").is_none();
        if is_response && let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
            let result = msg.get("result").cloned().filter(|v| !v.is_null());
            let error = msg
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            emit(CopilotEvent::Response { id, result, error });
            continue;
        }
        let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
        match method {
            "window/showMessage" | "window/logMessage" => {
                if let Some(params) = msg.get("params") {
                    let level = params.get("type").and_then(|v| v.as_u64()).unwrap_or(3);
                    let text = params.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    vlog!("copilot {} level={} {}", method, level, text);
                }
            }
            _ => {
                // $/progress and other notifications are ignored — the
                // editor has no UI for Copilot progress yet.
            }
        }
    }
}

/// Range of text the server wants the client to replace when the
/// inline suggestion is accepted. LSP positions are 0-based, half-open
/// at `end`. Treated as char counts here for parity with the rest of
/// the editor's cursor model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaceRange {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
}

/// First inline-completion item from a `textDocument/inlineCompletion`
/// response. `text` is the full server-side insertion verbatim — it
/// often *includes* characters the user has already typed, so the
/// caller pairs it with [`Self::range`] to compute the suffix to paint
/// as ghost text.
#[derive(Debug, Clone)]
pub struct InlineCompletionRaw {
    pub text: String,
    pub range: Option<ReplaceRange>,
}

/// Parse a `textDocument/inlineCompletion` response body into the
/// first item's `insertText` + `range`. Returns `None` when the
/// response carried no items or when the first item has no usable
/// `insertText`. The renderer / accept paths split work between the
/// two fields.
pub fn parse_inline_completion(result: &Value) -> Option<InlineCompletionRaw> {
    // The server can answer with either the raw item list (older
    // Copilot revisions) or `{items: [...]}` (current LSP 3.18 shape).
    let items = result
        .get("items")
        .and_then(|v| v.as_array())
        .or_else(|| result.as_array())?;
    let first = items.first()?;
    let text = first.get("insertText")?.as_str()?;
    if text.is_empty() {
        return None;
    }
    let range = first.get("range").and_then(parse_range);
    Some(InlineCompletionRaw {
        text: text.to_string(),
        range,
    })
}

/// Outcome of a `signInInitiate` reply. Either the device-flow code
/// the user has to paste into the browser, or "already signed in" —
/// Copilot uses the same method for both, gated on whether a valid
/// token is already present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInInitiate {
    PromptDeviceFlow {
        user_code: String,
        verification_uri: String,
    },
    AlreadySignedIn {
        user: Option<String>,
    },
}

pub fn parse_sign_in_initiate(result: &Value) -> Option<SignInInitiate> {
    let status = result.get("status").and_then(|v| v.as_str()).unwrap_or("");
    match status {
        "AlreadySignedIn" | "OK" => Some(SignInInitiate::AlreadySignedIn {
            user: result
                .get("user")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        }),
        // Default to device-flow when the server hands back the code
        // even if the status string is something we don't recognise —
        // the userCode is the load-bearing piece.
        _ => {
            let user_code = result.get("userCode")?.as_str()?.to_owned();
            let verification_uri = result.get("verificationUri")?.as_str()?.to_owned();
            Some(SignInInitiate::PromptDeviceFlow {
                user_code,
                verification_uri,
            })
        }
    }
}

/// Outcome of a `checkStatus` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    /// Token present and accepted — inline completion can fire.
    SignedIn { user: Option<String> },
    /// No token saved or the saved one is invalid. User needs to run
    /// the sign-in flow.
    NotSignedIn,
    /// Token present but the account isn't entitled to Copilot (no
    /// subscription, blocked by org policy, etc.). Same UX as
    /// not-signed-in for our purposes — completions won't fire.
    NotAuthorized { reason: Option<String> },
    /// Anything else the server reports. Treated as "don't request
    /// completions" to be safe.
    Other(String),
}

pub fn parse_check_status(result: &Value) -> Option<CheckStatus> {
    let status = result.get("status")?.as_str()?;
    let user = result
        .get("user")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    Some(match status {
        "OK" | "MaybeOK" => CheckStatus::SignedIn { user },
        "NotSignedIn" => CheckStatus::NotSignedIn,
        "NotAuthorized" => CheckStatus::NotAuthorized {
            reason: result
                .get("message")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        },
        other => CheckStatus::Other(other.to_string()),
    })
}

fn parse_range(v: &Value) -> Option<ReplaceRange> {
    let start = v.get("start")?;
    let end = v.get("end")?;
    Some(ReplaceRange {
        start_line: start.get("line")?.as_u64()? as u32,
        start_character: start.get("character")?.as_u64()? as u32,
        end_line: end.get("line")?.as_u64()? as u32,
        end_character: end.get("character")?.as_u64()? as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_picks_first_item_text() {
        let v = json!({
            "items": [
                { "insertText": "hello" },
                { "insertText": "world" }
            ]
        });
        let raw = parse_inline_completion(&v).expect("first item");
        assert_eq!(raw.text, "hello");
        assert!(raw.range.is_none());
    }

    #[test]
    fn parse_handles_bare_array() {
        let v = json!([{ "insertText": "abc" }]);
        assert_eq!(parse_inline_completion(&v).unwrap().text, "abc");
    }

    #[test]
    fn parse_none_on_empty_list() {
        let v = json!({ "items": [] });
        assert!(parse_inline_completion(&v).is_none());
    }

    #[test]
    fn parse_none_on_empty_text() {
        let v = json!({ "items": [{ "insertText": "" }] });
        assert!(parse_inline_completion(&v).is_none());
    }

    #[test]
    fn parse_none_on_missing_insert_text() {
        let v = json!({ "items": [{ "label": "foo" }] });
        assert!(parse_inline_completion(&v).is_none());
    }

    #[test]
    fn parse_sign_in_initiate_device_flow() {
        let v = json!({
            "status": "PromptUserDeviceFlow",
            "userCode": "ABCD-1234",
            "verificationUri": "https://github.com/login/device",
            "expiresIn": 900,
            "interval": 5
        });
        let s = parse_sign_in_initiate(&v).unwrap();
        assert_eq!(
            s,
            SignInInitiate::PromptDeviceFlow {
                user_code: "ABCD-1234".into(),
                verification_uri: "https://github.com/login/device".into(),
            }
        );
    }

    #[test]
    fn parse_sign_in_initiate_already_signed_in() {
        let v = json!({ "status": "AlreadySignedIn", "user": "octocat" });
        let s = parse_sign_in_initiate(&v).unwrap();
        assert_eq!(
            s,
            SignInInitiate::AlreadySignedIn {
                user: Some("octocat".into())
            }
        );
    }

    #[test]
    fn parse_sign_in_initiate_falls_back_to_device_flow_on_unknown_status() {
        let v = json!({
            "status": "SomethingNew",
            "userCode": "X",
            "verificationUri": "y"
        });
        let s = parse_sign_in_initiate(&v).unwrap();
        assert!(matches!(s, SignInInitiate::PromptDeviceFlow { .. }));
    }

    #[test]
    fn parse_sign_in_initiate_none_without_code() {
        let v = json!({ "status": "Other" });
        assert!(parse_sign_in_initiate(&v).is_none());
    }

    #[test]
    fn parse_check_status_signed_in() {
        let v = json!({ "status": "OK", "user": "octocat" });
        let s = parse_check_status(&v).unwrap();
        assert_eq!(
            s,
            CheckStatus::SignedIn {
                user: Some("octocat".into())
            }
        );
    }

    #[test]
    fn parse_check_status_maybeok_treated_as_signed_in() {
        let v = json!({ "status": "MaybeOK" });
        let s = parse_check_status(&v).unwrap();
        assert!(matches!(s, CheckStatus::SignedIn { user: None }));
    }

    #[test]
    fn parse_check_status_not_signed_in() {
        let v = json!({ "status": "NotSignedIn" });
        assert_eq!(parse_check_status(&v).unwrap(), CheckStatus::NotSignedIn);
    }

    #[test]
    fn parse_check_status_not_authorized_keeps_reason() {
        let v = json!({ "status": "NotAuthorized", "message": "no entitlement" });
        let s = parse_check_status(&v).unwrap();
        assert_eq!(
            s,
            CheckStatus::NotAuthorized {
                reason: Some("no entitlement".into())
            }
        );
    }

    #[test]
    fn parse_check_status_unknown_status_falls_into_other() {
        let v = json!({ "status": "WeirdNewState" });
        assert_eq!(
            parse_check_status(&v).unwrap(),
            CheckStatus::Other("WeirdNewState".into())
        );
    }

    #[test]
    fn parse_check_status_none_on_missing_status() {
        let v = json!({});
        assert!(parse_check_status(&v).is_none());
    }

    #[test]
    fn parse_extracts_range_when_present() {
        let v = json!({
            "items": [{
                "insertText": "fn hello() {}",
                "range": {
                    "start": { "line": 3, "character": 0 },
                    "end":   { "line": 3, "character": 8 }
                }
            }]
        });
        let raw = parse_inline_completion(&v).unwrap();
        assert_eq!(raw.text, "fn hello() {}");
        let range = raw.range.unwrap();
        assert_eq!(range.start_line, 3);
        assert_eq!(range.start_character, 0);
        assert_eq!(range.end_line, 3);
        assert_eq!(range.end_character, 8);
    }
}
