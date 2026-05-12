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
//! MVP scope: `initialize` handshake, full-document sync
//! (`didOpen`/`didChange`/`didClose`), and `publishDiagnostics`. Hover,
//! goto-definition, completion etc. are intentionally out of scope here.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::config::LspConfig;

mod coordinator;
pub use coordinator::{LspCoordinator, LspEventOutcome};

// ────────────────────────────────────────────────────────────────────────
// Public types
// ────────────────────────────────────────────────────────────────────────

/// Event delivered from a reader thread back to the App. Keyed by the
/// document URI the event applies to (when relevant) so the App can
/// route to the right buffer without knowing which client sent it.
#[derive(Debug, Clone)]
pub enum LspEvent {
    /// Server replaced the diagnostics for a document. An empty `items`
    /// vector means "clear".
    Diagnostics { uri: String, items: Vec<Diagnostic> },
    /// `window/showMessage` — surface in the status bar.
    Message { level: u8, text: String },
    /// Response to an earlier request we sent. `id` matches what
    /// [`LspClient::request`] returned; the App keeps a `(lang, id) →
    /// kind` map so it knows how to interpret `result`. `lang` is
    /// stamped by the reader thread so the App can disambiguate
    /// responses arriving from multiple servers on the same channel.
    Response {
        lang: String,
        id: u64,
        /// `None` when the server returned an error or a null result.
        result: Option<Value>,
        /// Server error message, if any.
        error: Option<String>,
    },
    /// Reader hit a fatal error and is exiting.
    Error(String),
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Severity,
    pub message: String,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Copy)]
pub struct Position {
    /// 0-based line.
    pub line: u32,
    /// 0-based UTF-16 character offset per spec. We treat it as a char
    /// index — close enough for ASCII source which is the common case.
    pub character: u32,
}

/// LSP `Location` — a span inside a single file. Used for definition /
/// references results.
#[derive(Debug, Clone)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

/// LSP `TextEdit` — replace `range` with `new_text`.
#[derive(Debug, Clone)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

/// Simplified LSP `WorkspaceEdit` — a flat map from document URI to the
/// edits to apply there. We accept both `changes` and `documentChanges`
/// shapes server-side and normalise into this view.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceEdit {
    pub changes: HashMap<String, Vec<TextEdit>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    fn from_code(c: i64) -> Severity {
        match c {
            1 => Severity::Error,
            2 => Severity::Warning,
            3 => Severity::Info,
            _ => Severity::Hint,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────
// Client
// ────────────────────────────────────────────────────────────────────────

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
                    "publishDiagnostics": { "relatedInformation": false }
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
            handle_server_request(&stdin, &msg);
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

// ────────────────────────────────────────────────────────────────────────
// Reader loop
// ────────────────────────────────────────────────────────────────────────

fn reader_loop(
    mut reader: BufReader<ChildStdout>,
    emit: Box<dyn Fn(LspEvent) + Send>,
    stdin: Arc<Mutex<ChildStdin>>,
    lang: String,
) {
    loop {
        let msg = match read_message(&mut reader) {
            Ok(m) => m,
            Err(e) => {
                emit(LspEvent::Error(format!("lsp reader: {}", e)));
                return;
            }
        };
        // Server-to-client request: has both `id` and `method`. The
        // server is blocked waiting for us, so reply right here.
        if msg.get("id").is_some() && msg.get("method").is_some() {
            handle_server_request(&stdin, &msg);
            continue;
        }
        // Response to a request we sent: has `id` but no `method`. The
        // initialize-response handshake is consumed synchronously before
        // this loop runs, so every response we see here is for a
        // post-handshake request.
        if msg.get("method").is_none()
            && let Some(id) = msg.get("id").and_then(|v| v.as_u64())
        {
            let result = msg.get("result").cloned().filter(|v| !v.is_null());
            let error = msg
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            emit(LspEvent::Response {
                lang: lang.clone(),
                id,
                result,
                error,
            });
            continue;
        }
        let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
        match method {
            "textDocument/publishDiagnostics" => {
                if let Some(params) = msg.get("params")
                    && let Some(ev) = parse_publish_diagnostics(params)
                {
                    emit(ev);
                }
            }
            "window/showMessage" | "window/logMessage" => {
                if let Some(params) = msg.get("params") {
                    let level = params.get("type").and_then(|v| v.as_u64()).unwrap_or(3) as u8;
                    let text = params
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    emit(LspEvent::Message { level, text });
                }
            }
            _ => {
                // Responses to outbound requests, $/progress and other
                // notifications we don't act on go unhandled for the MVP.
            }
        }
    }
}

/// Respond to a server-to-client request with a generic null result. This
/// is enough to unblock rust-analyzer's `client/registerCapability`,
/// `workspace/configuration`, and `window/workDoneProgress/create`
/// requests so that flycheck / file watching can proceed. Servers that
/// genuinely need a structured response will degrade gracefully.
fn handle_server_request(stdin: &Arc<Mutex<ChildStdin>>, msg: &Value) {
    let Some(id) = msg.get("id") else { return };
    let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
    // `workspace/configuration` expects an array of items mirroring the
    // request's `items` length — anything else trips a deserialise error
    // server-side. Match the shape but with null values.
    let result = if method == "workspace/configuration" {
        let n = msg
            .get("params")
            .and_then(|p| p.get("items"))
            .and_then(|i| i.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        Value::Array(vec![Value::Null; n])
    } else {
        Value::Null
    };
    let reply = json!({ "jsonrpc": "2.0", "id": id.clone(), "result": result });
    let _ = write_framed(stdin, &reply);
}

fn parse_publish_diagnostics(params: &Value) -> Option<LspEvent> {
    let uri = params.get("uri")?.as_str()?.to_string();
    let items = params.get("diagnostics")?.as_array()?;
    let mut out = Vec::with_capacity(items.len());
    for d in items {
        let range = d.get("range")?;
        let start = range.get("start")?;
        let end = range.get("end")?;
        let sev = Severity::from_code(d.get("severity").and_then(|v| v.as_i64()).unwrap_or(1));
        let message = d
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let source = d
            .get("source")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        out.push(Diagnostic {
            range: Range {
                start: Position {
                    line: start.get("line")?.as_u64().unwrap_or(0) as u32,
                    character: start.get("character")?.as_u64().unwrap_or(0) as u32,
                },
                end: Position {
                    line: end.get("line")?.as_u64().unwrap_or(0) as u32,
                    character: end.get("character")?.as_u64().unwrap_or(0) as u32,
                },
            },
            severity: sev,
            message,
            source,
        });
    }
    Some(LspEvent::Diagnostics { uri, items: out })
}

// ────────────────────────────────────────────────────────────────────────
// JSON-RPC framing
// ────────────────────────────────────────────────────────────────────────

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

fn write_message<W: Write>(w: &mut W, msg: &Value) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()?;
    Ok(())
}

/// Write a framed message through a locked stdin. Both the App thread
/// and the reader thread call this, so the lock guarantees that the
/// header + body of one message can't be interleaved with another's.
fn write_framed(stdin: &Arc<Mutex<ChildStdin>>, msg: &Value) -> Result<()> {
    let mut guard = stdin.lock().map_err(|_| anyhow!("lsp stdin poisoned"))?;
    write_message(&mut *guard, msg)
}

fn read_message<R: BufRead>(r: &mut R) -> Result<Value> {
    let mut content_length: Option<usize> = None;
    let mut header = String::new();
    loop {
        header.clear();
        let n = r.read_line(&mut header)?;
        if n == 0 {
            bail!("EOF from LSP server");
        }
        let line = header.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            content_length = Some(rest.trim().parse()?);
        }
        // Other headers (Content-Type) ignored.
    }
    let n = content_length.ok_or_else(|| anyhow!("missing Content-Length"))?;
    let mut body = vec![0u8; n];
    r.read_exact(&mut body)?;
    let v: Value = serde_json::from_slice(&body)?;
    Ok(v)
}

// ────────────────────────────────────────────────────────────────────────
// Path / URI helpers
// ────────────────────────────────────────────────────────────────────────

/// Inverse of [`path_to_uri`]: strip the `file://` scheme and decode
/// percent-escapes. Anything else (`http://`, `untitled:`) returns
/// `None` — we don't try to round-trip those.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // On Windows the spec is `file:///C:/...`; on Unix it's `file:///abs`.
    // Either way the byte after the scheme is `/` and we hand off the
    // remainder as-is (decoded).
    let decoded = percent_decode(rest);
    Some(PathBuf::from(decoded))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a `Location` (LSP shape). Returns `None` on schema mismatch.
pub fn parse_location(v: &Value) -> Option<Location> {
    let uri = v.get("uri").and_then(|x| x.as_str())?.to_string();
    let range = parse_range(v.get("range")?)?;
    Some(Location { uri, range })
}

/// Parse a `LocationLink` and reduce it to the same shape as `Location`
/// (taking `targetUri` + `targetSelectionRange`).
pub fn parse_location_link(v: &Value) -> Option<Location> {
    let uri = v.get("targetUri").and_then(|x| x.as_str())?.to_string();
    let range = parse_range(
        v.get("targetSelectionRange")
            .or_else(|| v.get("targetRange"))?,
    )?;
    Some(Location { uri, range })
}

/// `textDocument/definition` may answer with a single `Location`, a
/// single `LocationLink`, an array of either, or `null`. Normalise to a
/// flat `Vec<Location>`.
pub fn parse_locations(v: &Value) -> Vec<Location> {
    if v.is_null() {
        return Vec::new();
    }
    if let Some(arr) = v.as_array() {
        return arr
            .iter()
            .filter_map(|x| parse_location(x).or_else(|| parse_location_link(x)))
            .collect();
    }
    if let Some(loc) = parse_location(v).or_else(|| parse_location_link(v)) {
        return vec![loc];
    }
    Vec::new()
}

fn parse_range(v: &Value) -> Option<Range> {
    let start = v.get("start")?;
    let end = v.get("end")?;
    Some(Range {
        start: Position {
            line: start.get("line")?.as_u64().unwrap_or(0) as u32,
            character: start.get("character")?.as_u64().unwrap_or(0) as u32,
        },
        end: Position {
            line: end.get("line")?.as_u64().unwrap_or(0) as u32,
            character: end.get("character")?.as_u64().unwrap_or(0) as u32,
        },
    })
}

/// Parse a `WorkspaceEdit`. Both legacy `changes: { uri: [TextEdit] }`
/// and modern `documentChanges: [TextDocumentEdit]` shapes are
/// flattened into the same map. We ignore document `create`/`rename`/
/// `delete` operations — rename refactors that need them are out of
/// scope for now.
pub fn parse_workspace_edit(v: &Value) -> Option<WorkspaceEdit> {
    if v.is_null() {
        return None;
    }
    let mut out: HashMap<String, Vec<TextEdit>> = HashMap::new();
    if let Some(changes) = v.get("changes").and_then(|c| c.as_object()) {
        for (uri, edits) in changes {
            if let Some(arr) = edits.as_array() {
                let edits = arr.iter().filter_map(parse_text_edit).collect();
                out.insert(uri.clone(), edits);
            }
        }
    }
    if let Some(doc_changes) = v.get("documentChanges").and_then(|c| c.as_array()) {
        for dc in doc_changes {
            // A `TextDocumentEdit` has `textDocument.uri` + `edits[]`. Other
            // operations (`CreateFile`, `RenameFile`, `DeleteFile`) have
            // their own `kind` field — skip those.
            if dc.get("kind").is_some() {
                continue;
            }
            let Some(uri) = dc
                .get("textDocument")
                .and_then(|td| td.get("uri"))
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let Some(edits) = dc.get("edits").and_then(|e| e.as_array()) else {
                continue;
            };
            let edits: Vec<TextEdit> = edits.iter().filter_map(parse_text_edit).collect();
            out.entry(uri.to_string()).or_default().extend(edits);
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(WorkspaceEdit { changes: out })
}

fn parse_text_edit(v: &Value) -> Option<TextEdit> {
    let range = parse_range(v.get("range")?)?;
    let new_text = v.get("newText")?.as_str()?.to_string();
    Some(TextEdit { range, new_text })
}

/// Apply a list of [`TextEdit`]s to an in-memory `Vec<String>`
/// (one entry per buffer line). Edits are sorted in **reverse**
/// document order before being applied so earlier positions stay
/// valid as we splice. Out-of-range edits are clamped to the buffer
/// rather than panicking — a stale server response shouldn't crash us.
pub fn apply_text_edits(lines: &mut Vec<String>, mut edits: Vec<TextEdit>) {
    edits.sort_by(|a, b| {
        b.range
            .start
            .line
            .cmp(&a.range.start.line)
            .then_with(|| b.range.start.character.cmp(&a.range.start.character))
    });
    for edit in edits {
        apply_one_edit(lines, &edit);
    }
}

fn apply_one_edit(lines: &mut Vec<String>, edit: &TextEdit) {
    if lines.is_empty() {
        lines.push(String::new());
    }
    let last = lines.len() - 1;
    let s_row = (edit.range.start.line as usize).min(last);
    let e_row = (edit.range.end.line as usize).min(last);
    let s_col_chars = edit.range.start.character as usize;
    let e_col_chars = edit.range.end.character as usize;
    let prefix: String = lines[s_row].chars().take(s_col_chars).collect();
    let suffix: String = lines[e_row].chars().skip(e_col_chars).collect();
    let new_lines: Vec<&str> = edit.new_text.split('\n').collect();
    let replacement: Vec<String> = if new_lines.len() == 1 {
        vec![format!("{}{}{}", prefix, new_lines[0], suffix)]
    } else {
        let mut v = Vec::with_capacity(new_lines.len());
        v.push(format!("{}{}", prefix, new_lines[0]));
        for &mid in &new_lines[1..new_lines.len() - 1] {
            v.push(mid.to_string());
        }
        v.push(format!("{}{}", new_lines[new_lines.len() - 1], suffix));
        v
    };
    lines.splice(s_row..=e_row, replacement);
}

/// Best-effort `file://` URI for a path. Non-UTF-8 paths fall back to a
/// lossy conversion — we don't need bit-perfect roundtrip, just something
/// the server can match against.
pub fn path_to_uri(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let s = abs.to_string_lossy();
    if s.starts_with('/') {
        format!("file://{}", s)
    } else {
        format!("file:///{}", s)
    }
}

/// Walk up from `start_dir` looking for the first directory that contains
/// any of `markers`. Falls back to `start_dir` itself when nothing matches.
///
/// We canonicalize first because `Path::parent()` only strips a trailing
/// component — for a relative path like `src/main.rs` it'd bottom out at
/// `""` after one step instead of climbing into the real filesystem,
/// which would cause us to report a workspace root that doesn't contain
/// the marker (and rust-analyzer to fail with "Failed to discover
/// workspace").
pub fn find_root_upward(start_dir: &Path, markers: &[String]) -> PathBuf {
    let abs = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());
    if markers.is_empty() {
        return abs;
    }
    let mut cur: &Path = &abs;
    loop {
        if markers.iter().any(|m| cur.join(m).exists()) {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return abs.clone(),
        }
    }
}

/// Resolve a workspace root for `file` against a stable anchor `cwd`.
///
/// Strategy:
/// 1. If `markers` is empty, return canonicalized `cwd`.
/// 2. If `cwd` itself contains a marker, return it.
/// 3. BFS from `cwd` into subdirectories (capped depth, common build /
///    VCS dirs skipped) for a marker. First match wins.
/// 4. If `file` is provided **and** lives outside `cwd`'s subtree, fall
///    back to [`find_root_upward`] from the file's parent — that covers
///    `vorto ../other_project/main.rs` from an unrelated cwd.
/// 5. Otherwise return canonicalized `cwd`.
///
/// We deliberately don't walk **up** from `cwd`. The user being in this
/// directory is a signal; escaping it could land on a monorepo parent
/// or other unrelated workspace.
pub fn discover_root(cwd: &Path, file: Option<&Path>, markers: &[String]) -> PathBuf {
    let cwd_abs = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    if markers.is_empty() {
        return cwd_abs;
    }
    if markers.iter().any(|m| cwd_abs.join(m).exists()) {
        return cwd_abs;
    }
    if let Some(found) = bfs_for_marker(&cwd_abs, markers) {
        return found;
    }
    if let Some(file) = file {
        let file_abs = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        if !file_abs.starts_with(&cwd_abs)
            && let Some(parent) = file_abs.parent()
        {
            return find_root_upward(parent, markers);
        }
    }
    cwd_abs
}

/// Max directory depth scanned by [`discover_root`]'s descent. Chosen to
/// cover typical monorepo layouts (`apps/<name>/Cargo.toml`,
/// `packages/<name>/package.json`) without melting on huge trees.
const DESCEND_MAX_DEPTH: usize = 6;

/// Directories skipped during descent — anything noisy, generated, or
/// containing nested dependency manifests we don't want to mistake for
/// the user's own project root.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".direnv",
    ".cache",
    ".idea",
    ".vscode",
];

fn bfs_for_marker(root: &Path, markers: &[String]) -> Option<PathBuf> {
    use std::collections::VecDeque;
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));
    while let Some((dir, depth)) = queue.pop_front() {
        if depth > 0 && markers.iter().any(|m| dir.join(m).exists()) {
            return Some(dir);
        }
        if depth >= DESCEND_MAX_DEPTH {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            // Skip all dotdirs — keeps results predictable and avoids
            // wandering into `.git`/`.cache`/etc. that we'd otherwise
            // have to enumerate by name.
            if name_s.starts_with('.') {
                continue;
            }
            if SKIP_DIRS.iter().any(|d| *d == name_s) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                queue.push_back((path, depth + 1));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn framing_roundtrip() {
        let mut buf: Vec<u8> = Vec::new();
        let msg = json!({ "jsonrpc": "2.0", "method": "hi", "params": {"x": 1} });
        write_message(&mut buf, &msg).unwrap();
        // Header must precede body and Content-Length must match.
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.starts_with("Content-Length: "));
        assert!(s.contains("\r\n\r\n"));

        let mut r = Cursor::new(buf);
        let parsed = read_message(&mut r).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn parse_diagnostics_basic() {
        let params = json!({
            "uri": "file:///foo.rs",
            "diagnostics": [{
                "range": {
                    "start": { "line": 2, "character": 4 },
                    "end":   { "line": 2, "character": 7 }
                },
                "severity": 1,
                "message": "boom",
                "source": "rust-analyzer"
            }]
        });
        let ev = parse_publish_diagnostics(&params).unwrap();
        let LspEvent::Diagnostics { uri, items } = ev else {
            panic!("wrong variant");
        };
        assert_eq!(uri, "file:///foo.rs");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, Severity::Error);
        assert_eq!(items[0].message, "boom");
        assert_eq!(items[0].source.as_deref(), Some("rust-analyzer"));
        assert_eq!(items[0].range.start.line, 2);
    }

    #[test]
    fn find_root_upward_walks_to_marker() {
        let tmp = std::env::temp_dir().join(format!("vorto-lsp-{}", std::process::id()));
        let inner = tmp.join("a/b/c");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "").unwrap();
        let root = find_root_upward(&inner, &["Cargo.toml".to_string()]);
        // Compare canonicalised — temp dirs on macOS resolve via /private.
        assert_eq!(root, tmp.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_root_upward_handles_relative_path() {
        // The pre-fix bug: a relative path bottomed out at "" after one
        // parent() step and reported the start dir instead of climbing.
        let tmp = std::env::temp_dir().join(format!("vorto-lsp-rel-{}", std::process::id()));
        let inner = tmp.join("nested");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "").unwrap();
        let root = find_root_upward(&inner, &["Cargo.toml".to_string()]);
        assert_eq!(root, tmp.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_root_picks_cwd_when_marker_at_cwd() {
        let tmp = std::env::temp_dir().join(format!("vorto-disc1-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "").unwrap();
        let root = discover_root(&tmp, None, &["Cargo.toml".to_string()]);
        assert_eq!(root, tmp.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_root_descends_into_subdir() {
        // cwd has no Cargo.toml; one of its grandchildren does. BFS must
        // surface that nested project.
        let tmp = std::env::temp_dir().join(format!("vorto-disc2-{}", std::process::id()));
        let nested = tmp.join("apps/foo");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Cargo.toml"), "").unwrap();
        let root = discover_root(&tmp, None, &["Cargo.toml".to_string()]);
        assert_eq!(root, nested.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_root_falls_back_to_cwd_when_no_marker() {
        let tmp = std::env::temp_dir().join(format!("vorto-disc3-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let root = discover_root(&tmp, None, &["Cargo.toml".to_string()]);
        assert_eq!(root, tmp.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discover_root_walks_up_for_outside_file() {
        // cwd is empty; the file lives in a separate tree that does have
        // a marker further up. Fall through to upward walk from the
        // file's parent rather than reporting cwd.
        let tmp = std::env::temp_dir().join(format!("vorto-disc4-{}", std::process::id()));
        let other = std::env::temp_dir().join(format!("vorto-disc4other-{}", std::process::id()));
        let nested = other.join("src");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(other.join("Cargo.toml"), "").unwrap();
        let file = nested.join("main.rs");
        std::fs::write(&file, "").unwrap();
        let root = discover_root(&tmp, Some(&file), &["Cargo.toml".to_string()]);
        assert_eq!(root, other.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn discover_root_skips_target_dir() {
        // Make sure descent doesn't dive into `target/` etc. where vendored
        // crates can have their own Cargo.toml.
        let tmp = std::env::temp_dir().join(format!("vorto-disc5-{}", std::process::id()));
        let bogus = tmp.join("target/debug/some_crate");
        std::fs::create_dir_all(&bogus).unwrap();
        std::fs::write(bogus.join("Cargo.toml"), "").unwrap();
        let root = discover_root(&tmp, None, &["Cargo.toml".to_string()]);
        // Should fall back to cwd, NOT find the Cargo.toml under target/.
        assert_eq!(root, tmp.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn severity_from_code_defaults_to_hint() {
        assert_eq!(Severity::from_code(1), Severity::Error);
        assert_eq!(Severity::from_code(4), Severity::Hint);
        assert_eq!(Severity::from_code(99), Severity::Hint);
    }

    #[test]
    fn parse_locations_handles_all_shapes() {
        // Single Location object.
        let single = json!({
            "uri": "file:///a.rs",
            "range": { "start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5} }
        });
        let v = parse_locations(&single);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].uri, "file:///a.rs");

        // Array of Locations.
        let arr = json!([single]);
        assert_eq!(parse_locations(&arr).len(), 1);

        // LocationLink shape.
        let link = json!([{
            "targetUri": "file:///b.rs",
            "targetSelectionRange": {
                "start": {"line": 0, "character": 0},
                "end":   {"line": 0, "character": 3}
            }
        }]);
        let v = parse_locations(&link);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].uri, "file:///b.rs");

        // null → empty.
        assert!(parse_locations(&Value::Null).is_empty());
    }

    #[test]
    fn percent_decode_handles_spaces() {
        assert_eq!(percent_decode("foo%20bar"), "foo bar");
        assert_eq!(percent_decode("plain"), "plain");
        // Truncated escape stays literal.
        assert_eq!(percent_decode("foo%"), "foo%");
    }

    #[test]
    fn uri_to_path_strips_scheme_and_decodes() {
        let p = uri_to_path("file:///tmp/with%20space.rs").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/with space.rs"));
        assert!(uri_to_path("https://example.com/x").is_none());
    }

    #[test]
    fn apply_text_edits_single_line_replace() {
        let mut lines = vec!["let foo = 1;".to_string()];
        let edits = vec![TextEdit {
            range: Range {
                start: Position {
                    line: 0,
                    character: 4,
                },
                end: Position {
                    line: 0,
                    character: 7,
                },
            },
            new_text: "bar".to_string(),
        }];
        apply_text_edits(&mut lines, edits);
        assert_eq!(lines, vec!["let bar = 1;".to_string()]);
    }

    #[test]
    fn apply_text_edits_order_independent() {
        // Two edits on the same line — the apply step must process them
        // right-to-left so the earlier edit doesn't shift the later one.
        let mut lines = vec!["aaa bbb ccc".to_string()];
        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 3,
                    },
                },
                new_text: "XXXX".to_string(),
            },
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 8,
                    },
                    end: Position {
                        line: 0,
                        character: 11,
                    },
                },
                new_text: "Y".to_string(),
            },
        ];
        apply_text_edits(&mut lines, edits);
        assert_eq!(lines, vec!["XXXX bbb Y".to_string()]);
    }

    #[test]
    fn parse_workspace_edit_normalises_both_shapes() {
        // Legacy `changes` map.
        let v = json!({
            "changes": {
                "file:///a.rs": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end":   {"line": 0, "character": 3}
                    },
                    "newText": "X"
                }]
            }
        });
        let edit = parse_workspace_edit(&v).unwrap();
        assert_eq!(edit.changes.len(), 1);
        assert_eq!(edit.changes["file:///a.rs"].len(), 1);

        // Modern `documentChanges` array.
        let v = json!({
            "documentChanges": [{
                "textDocument": { "uri": "file:///b.rs", "version": 1 },
                "edits": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end":   {"line": 0, "character": 3}
                    },
                    "newText": "Y"
                }]
            }]
        });
        let edit = parse_workspace_edit(&v).unwrap();
        assert_eq!(edit.changes["file:///b.rs"][0].new_text, "Y");

        assert!(parse_workspace_edit(&Value::Null).is_none());
    }
}
