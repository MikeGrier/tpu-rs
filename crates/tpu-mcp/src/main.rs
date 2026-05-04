// Copyright (c) 2026, Michael Grier

//! `tpu-mcp` — MCP (Model Context Protocol) server that exposes `tpu`'s
//! file-processing capabilities as tools callable by AI agents such as
//! GitHub Copilot.
//!
//! The server speaks JSON-RPC 2.0 over stdio using newline-delimited messages.
//! Each tool invocation calls `tpu` library functions directly (via the `tpu`
//! crate) so that argument values — even those starting with `--` — are never
//! misinterpreted as CLI options.

mod tools;

use std::io::{self, BufRead};

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── JSON-RPC 2.0 wire types ───────────────────────────────────────────────────

/// An incoming JSON-RPC 2.0 message (request or notification).
#[derive(Deserialize)]
struct Message {
    #[allow(dead_code)]
    jsonrpc: String,
    /// Absent for notifications; present for requests.
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

/// An outgoing JSON-RPC 2.0 response.
#[derive(Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Value,
    #[serde(flatten)]
    body: ResponseBody,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ResponseBody {
    Ok { result: Value },
    Err { error: RpcError },
}

#[derive(Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

/// JSON-RPC 2.0 reserved error codes.
mod code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
}

// ── event loop ────────────────────────────────────────────────────────────────

fn main() {
    let config = parse_config();

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Emit a startup banner via the MCP logging-notification channel so it
    // appears in the client's MCP output as `info`, not `warning`. (VS Code
    // tags every line on stderr as `[warning]`.)
    log_info(
        &mut out,
        format!(
            "tpu-mcp {ver} starting (pid={pid})",
            ver = env!("CARGO_PKG_VERSION"),
            pid = std::process::id(),
        ),
    );

    let names = tools::tool_names();
    let quoted: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
    log_info(
        &mut out,
        format!("advertising {} tools: {}", names.len(), quoted.join(", ")),
    );

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("tpu-mcp: stdin error: {e}");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let msg: Message = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(e) => {
                send_error(
                    &mut out,
                    Value::Null,
                    code::PARSE_ERROR,
                    format!("parse error: {e}"),
                );
                continue;
            }
        };

        // Notifications have no id — no response is sent.
        let id = match msg.id {
            None => continue,
            Some(ref v) if v.is_null() => continue,
            Some(v) => v,
        };

        let body = dispatch(msg.method.as_str(), msg.params, &config);
        if config.trace {
            log_info(&mut out, format!("dispatched '{}'", msg.method));
        }
        send_response(
            &mut out,
            Response {
                jsonrpc: "2.0",
                id,
                body,
            },
        );
    }
}

// ── dispatch ──────────────────────────────────────────────────────────────────

fn dispatch(method: &str, params: Option<Value>, config: &tools::ServerConfig) -> ResponseBody {
    match method {
        "initialize" => ResponseBody::Ok {
            result: serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "tpu-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        },

        "tools/list" => ResponseBody::Ok {
            result: serde_json::json!({ "tools": tools::list() }),
        },

        "tools/call" => {
            let params = match params {
                Some(p) => p,
                None => {
                    return ResponseBody::Err {
                        error: RpcError {
                            code: code::INVALID_PARAMS,
                            message: "tools/call requires params".into(),
                        },
                    }
                }
            };

            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));

            match tools::call(name, &args, config) {
                Ok(text) => ResponseBody::Ok {
                    result: serde_json::json!({
                        "content": [{ "type": "text", "text": text }]
                    }),
                },
                Err(e) => ResponseBody::Ok {
                    result: serde_json::json!({
                        "content": [{ "type": "text", "text": format!("error: {e}") }],
                        "isError": true
                    }),
                },
            }
        }

        "ping" => ResponseBody::Ok {
            result: serde_json::json!({}),
        },

        // MCP shutdown: client signals intent to close; acknowledge and let
        // the client close the transport.  We continue processing until stdin
        // closes so the client can still receive the response.
        "shutdown" => ResponseBody::Ok {
            result: Value::Null,
        },

        _ => ResponseBody::Err {
            error: RpcError {
                code: code::METHOD_NOT_FOUND,
                message: format!("method not found: {method}"),
            },
        },
    }
}

// ── I/O helpers ───────────────────────────────────────────────────────────────

fn send_response(out: &mut impl io::Write, response: Response) {
    match serde_json::to_string(&response) {
        Ok(mut s) => {
            s.push('\n');
            let _ = out.write_all(s.as_bytes());
            let _ = out.flush();
        }
        Err(e) => eprintln!("tpu-mcp: serialization error: {e}"),
    }
}

fn send_error(out: &mut impl io::Write, id: Value, code: i32, message: String) {
    send_response(
        out,
        Response {
            jsonrpc: "2.0",
            id,
            body: ResponseBody::Err {
                error: RpcError { code, message },
            },
        },
    );
}

/// Send an MCP `notifications/message` JSON-RPC notification on stdout.
///
/// Per the MCP spec, the server may send these to surface log output to the
/// client. VS Code displays them in the per-server MCP output channel using
/// the supplied `level` (`info`, `warning`, `error`, …). This is the correct
/// way for a stdio-transport server to emit user-facing diagnostics: writing
/// to stderr causes VS Code to tag every line as `[warning]` regardless of
/// intent, and writing to stdout outside the JSON-RPC framing would corrupt
/// the protocol.
fn send_notification(out: &mut impl io::Write, method: &str, params: Value) {
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    match serde_json::to_string(&msg) {
        Ok(mut s) => {
            s.push('\n');
            let _ = out.write_all(s.as_bytes());
            let _ = out.flush();
        }
        Err(e) => eprintln!("tpu-mcp: notification serialization error: {e}"),
    }
}

/// Send an `info`-level log notification with `logger = "tpu-mcp"`.
fn log_info(out: &mut impl io::Write, message: impl Into<String>) {
    send_notification(
        out,
        "notifications/message",
        serde_json::json!({
            "level": "info",
            "logger": "tpu-mcp",
            "data": message.into(),
        }),
    );
}

// -- startup configuration ----------------------------------------------------

/// Parse `--verify-delay-ms=N` and `--quiet` from the process arguments.
///
/// All unrecognised arguments are silently ignored so the server remains
/// forward-compatible with future flags without breaking existing `mcp.json`
/// configurations. `--quiet` may also be enabled by setting the
/// `TPU_MCP_QUIET` environment variable to a non-empty value.
fn parse_config() -> tools::ServerConfig {
    let mut verify_delay_ms: u64 = 100;
    let mut quiet: bool = std::env::var_os("TPU_MCP_QUIET")
        .is_some_and(|v| !v.is_empty());
    for arg in std::env::args_os().skip(1) {
        let s = arg.to_string_lossy();
        if let Some(rest) = s.strip_prefix("--verify-delay-ms=") {
            if let Ok(n) = rest.parse::<u64>() {
                verify_delay_ms = n;
            } else {
                eprintln!("tpu-mcp: ignoring invalid --verify-delay-ms value: {rest:?}");
            }
        } else if s == "--quiet" {
            quiet = true;
        }
    }
    tools::ServerConfig {
        verify_delay_ms,
        trace: !quiet,
    }
}
