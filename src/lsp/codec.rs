//! JSON-RPC framing and the per-client reader thread.
//!
//! The codec layer is intentionally narrow: it knows how to frame
//! messages on stdin and parse them off stdout, plus the inbound
//! dispatch (`reader_loop`) that lifts wire shapes into [`LspEvent`]s.

use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, ChildStdout};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::types::{Diagnostic, LspEvent, Position, Range, Severity};
use super::{BlockingPending, BlockingReply};

pub(super) fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub(super) fn notification(method: &str, params: Value) -> Value {
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
pub(super) fn write_framed(stdin: &Arc<Mutex<ChildStdin>>, msg: &Value) -> Result<()> {
    let mut guard = stdin.lock().map_err(|_| anyhow!("lsp stdin poisoned"))?;
    write_message(&mut *guard, msg)
}

pub(super) fn read_message<R: BufRead>(r: &mut R) -> Result<Value> {
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

pub(super) fn reader_loop(
    mut reader: BufReader<ChildStdout>,
    emit: Box<dyn Fn(LspEvent) + Send>,
    stdin: Arc<Mutex<ChildStdin>>,
    client: String,
    blocking_pending: BlockingPending,
) {
    loop {
        let msg = match read_message(&mut reader) {
            Ok(m) => m,
            Err(e) => {
                emit(LspEvent::Error {
                    client: client.clone(),
                    message: format!("lsp reader: {}", e),
                });
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
        let is_response = msg.get("method").is_none();
        if is_response && let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
            let result = msg.get("result").cloned().filter(|v| !v.is_null());
            let error = msg
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            // Blocking waiters (format-on-save) take precedence over
            // the async event channel. We pop from the pending map
            // under lock so a sibling timeout can't race us.
            let blocking_waiter = blocking_pending.lock().ok().and_then(|mut g| g.remove(&id));
            if let Some(tx) = blocking_waiter {
                let reply = match error {
                    Some(msg) => BlockingReply::Err(msg),
                    None => BlockingReply::Ok(result.unwrap_or(Value::Null)),
                };
                let _ = tx.send(reply);
                continue;
            }
            emit(LspEvent::Response {
                client: client.clone(),
                id,
                result,
                error,
            });
            continue;
        }
        let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
        match method {
            "textDocument/publishDiagnostics" => {
                if let Some(ev) = msg
                    .get("params")
                    .and_then(|p| parse_publish_diagnostics(&client, p))
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
pub(super) fn handle_server_request(stdin: &Arc<Mutex<ChildStdin>>, msg: &Value) {
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

fn parse_publish_diagnostics(client: &str, params: &Value) -> Option<LspEvent> {
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
    Some(LspEvent::Diagnostics {
        client: client.to_string(),
        uri,
        items: out,
    })
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
        let ev = parse_publish_diagnostics("rust::rust-analyzer", &params).unwrap();
        let LspEvent::Diagnostics { client, uri, items } = ev else {
            panic!("wrong variant");
        };
        assert_eq!(client, "rust::rust-analyzer");
        assert_eq!(uri, "file:///foo.rs");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, Severity::Error);
        assert_eq!(items[0].message, "boom");
        assert_eq!(items[0].source.as_deref(), Some("rust-analyzer"));
        assert_eq!(items[0].range.start.line, 2);
    }
}
