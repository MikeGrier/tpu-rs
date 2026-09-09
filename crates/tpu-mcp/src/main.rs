// Copyright (c) 2026, Michael Grier

//! `tpu-mcp` — MCP (Model Context Protocol) server that exposes `tpu`'s
//! file-processing capabilities as tools callable by AI agents such as
//! GitHub Copilot.
//!
//! The server speaks JSON-RPC 2.0 over stdio using newline-delimited messages.
//! Each tool invocation calls `tpu` library functions directly (via the `tpu`
//! crate) so that argument values — even those starting with `--` — are never
//! misinterpreted as CLI options.
//!
//! ## I/O worker mode (`--io-worker`)
//!
//! When invoked with [`worker::WORKER_ARG`] on its command line, the binary
//! re-enters as a private subprocess of the MCP server, reading JSON
//! requests from stdin and writing JSON responses to stdout (see
//! [`worker::run_worker`]).  This is the out-of-process I/O isolation path
//! used by default on Windows to keep Defender-induced process kills from
//! taking down the MCP session.

mod tools;
mod worker;

use std::{
    io::{self, BufRead},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use worker::IoWorkerHandle;

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
    // I/O-worker mode short-circuits everything else: the process becomes a
    // private child of the parent MCP server, talking the simple JSON wire
    // protocol defined in `worker.rs` and never touching the MCP framing.
    if std::env::args_os().any(|a| a == worker::WORKER_ARG) {
        worker::run_worker();
    }
    let (config, startup_warnings) = parse_config();

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

    // Out-of-process I/O isolation: default on Windows, opt-in elsewhere.
    // Disabled by `--no-io-worker` or `TPU_MCP_NO_IO_WORKER=1`.
    let io_worker = Arc::new(if config.io_worker_enabled {
        log_info(
            &mut out,
            "io worker enabled (out-of-process file I/O for fault isolation)",
        );
        IoWorkerHandle::enabled()
    } else {
        IoWorkerHandle::disabled()
    });

    // Replay any warnings collected during CLI parsing through the same
    // log channel (warning level) now that the stdout writer is available.
    for w in startup_warnings {
        log_warn(&mut out, w);
    }

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
        let id = match request_id(msg.id) {
            Some(v) => v,
            None => continue,
        };

        let body = dispatch(
            msg.method.as_str(),
            msg.params,
            &config,
            &io_worker,
            &mut out,
        );
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

/// Decide whether an incoming JSON-RPC message's `id` field identifies it
/// as a request needing a response, and if so extract that id.
///
/// Per JSON-RPC 2.0, a message with no `id` member — or with an `id` that is
/// explicitly `null` — is a *notification*: the server MUST NOT send a
/// response for it. Returns `None` in both of those cases; returns
/// `Some(id)` for any other (request) value.
///
/// This is split out from the event loop body as a small pure function so
/// it can be unit-tested directly against in-memory `Option<Value>` inputs
/// rather than only through a full stdio round trip (where a mutation to
/// the `is_null()` check previously showed up as a `cargo mutants` TIMEOUT:
/// misclassifying a notification as a request causes the server to attempt
/// a response the client never expects and isn't reading for, which can
/// block the writer once the pipe buffer fills).
fn request_id(id: Option<Value>) -> Option<Value> {
    match id {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => Some(v),
    }
}

// ── dispatch ──────────────────────────────────────────────────────────────────

fn dispatch(
    method: &str,
    params: Option<Value>,
    config: &tools::ServerConfig,
    io_worker: &IoWorkerHandle,
    out: &mut impl io::Write,
) -> ResponseBody {
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
                    };
                }
            };

            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));

            // Try the out-of-process worker first; fall back to in-process
            // execution only after the worker subsystem has exhausted its
            // retry budget.  Worker turbulence is surfaced to the client
            // via MCP `notifications/message` (warning level) so the user
            // can see it in their chat UI without consulting stderr.
            let outcome = {
                let mut warn = |msg: &str| log_warn(out, msg.to_string());
                io_worker.try_call(name, &args, config, &mut warn)
            };
            let result: Result<tools::ToolResult, Box<dyn std::error::Error>> = match outcome {
                Some(tr) => Ok(tr),
                None => tools::call(name, &args, config),
            };

            match result {
                Ok(tools::ToolResult { text, is_error }) => {
                    if is_error {
                        log_warn(out, format!("tool '{name}' error (in NDJSON response)"));
                    }
                    ResponseBody::Ok {
                        result: serde_json::json!({
                            "content": [{ "type": "text", "text": text }],
                            "isError": is_error
                        }),
                    }
                }
                Err(e) => {
                    // Surface a server-side log entry for tool failures so
                    // operators have a trail in the MCP output channel even
                    // when the client only displays the `isError` payload.
                    log_warn(out, format!("tool '{name}' failed: {e}"));
                    ResponseBody::Ok {
                        result: serde_json::json!({
                            "content": [{ "type": "text", "text": format!("error: {e}") }],
                            "isError": true
                        }),
                    }
                }
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

/// Send a `warning`-level log notification with `logger = "tpu-mcp"`.
fn log_warn(out: &mut impl io::Write, message: impl Into<String>) {
    send_notification(
        out,
        "notifications/message",
        serde_json::json!({
            "level": "warning",
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
///
/// Returns the parsed config plus any deferred warning messages collected
/// during parsing. Callers should emit these via [`log_warn`] *after* the
/// stdout writer is available so they appear in the client's MCP output
/// channel at `warning` level rather than as `[warning]`-tagged stderr.
fn parse_config() -> (tools::ServerConfig, Vec<String>) {
    let verify_delay_ms: u64 = 100;
    let quiet: bool = std::env::var_os("TPU_MCP_QUIET").is_some_and(|v| !v.is_empty());
    // Out-of-process I/O isolation: default on Windows, off elsewhere.
    // The env var lets users disable it without editing `mcp.json`.
    let io_worker_enabled: bool =
        cfg!(windows) && std::env::var_os("TPU_MCP_NO_IO_WORKER").is_none_or(|v| v.is_empty());
    // Retained for compatibility with older extension/server combinations.
    let eol_normalize: bool = std::env::var_os("TPU_EOL_NORMALIZE").is_some_and(|v| !v.is_empty());
    let mut warnings: Vec<String> = Vec::new();
    // Default walk-error policy for tools that traverse trees (find, copy).
    // Honour the env var first so the VS Code extension can plumb the user
    // setting in without needing a CLI flag at all; the CLI flag wins if
    // explicitly supplied.
    let default_on_error = match std::env::var("TPU_DEFAULT_ERROR_MODE").ok().as_deref() {
        Some("strict") | Some("fail") => tpu::cmd::copy::OnError::Fail,
        Some("continue") | Some("warn") => tpu::cmd::copy::OnError::Warn,
        Some(other) if !other.is_empty() => {
            warnings.push(format!(
                "ignoring unrecognised TPU_DEFAULT_ERROR_MODE value: {other:?}"
            ));
            tpu::cmd::copy::OnError::Warn
        }
        _ => tpu::cmd::copy::OnError::Warn,
    };
    let progress_detail = match std::env::var("TPU_PROGRESS_DETAIL").ok().as_deref() {
        Some("summary") => tools::ProgressDetail::Summary,
        Some("each-file") | Some("each_file") => tools::ProgressDetail::EachFile,
        Some(other) if !other.is_empty() => {
            warnings.push(format!(
                "ignoring unrecognised TPU_PROGRESS_DETAIL value: {other:?}"
            ));
            tools::ProgressDetail::EachFile
        }
        _ => tools::ProgressDetail::EachFile,
    };
    let mut flags = ArgFlags {
        verify_delay_ms,
        default_on_error,
        progress_detail,
        quiet,
        eol_normalize,
        io_worker_enabled,
        warnings,
    };
    for arg in std::env::args_os().skip(1) {
        apply_arg(&mut flags, &arg.to_string_lossy());
    }
    (
        tools::ServerConfig {
            verify_delay_ms: flags.verify_delay_ms,
            trace: !flags.quiet,
            default_on_error: flags.default_on_error,
            progress_detail: flags.progress_detail,
            io_worker_enabled: flags.io_worker_enabled,
            eol_normalize: flags.eol_normalize,
        },
        flags.warnings,
    )
}

/// The subset of [`parse_config`]'s state that a single CLI argument can
/// affect, threaded through [`apply_arg`].
struct ArgFlags {
    verify_delay_ms: u64,
    default_on_error: tpu::cmd::copy::OnError,
    progress_detail: tools::ProgressDetail,
    quiet: bool,
    eol_normalize: bool,
    io_worker_enabled: bool,
    warnings: Vec<String>,
}

/// Apply one already-decoded CLI argument to `flags`, exactly as `parse_config`'s
/// former inline loop body did.
///
/// Split out as a pure function over a plain `&str` (rather than reading
/// `std::env::args_os()` directly) so the argument-matching logic --
/// including the exact string comparisons for `--quiet`/`--eol-normalize`/
/// the worker sentinel args -- can be unit-tested with synthetic argument
/// strings instead of only through a real process invocation.
fn apply_arg(flags: &mut ArgFlags, s: &str) {
    if let Some(rest) = s.strip_prefix("--verify-delay-ms=") {
        if let Ok(n) = rest.parse::<u64>() {
            flags.verify_delay_ms = n;
        } else {
            flags.warnings.push(format!(
                "ignoring invalid --verify-delay-ms value: {rest:?}"
            ));
        }
    } else if let Some(rest) = s.strip_prefix("--default-on-error=") {
        match rest {
            "warn" | "continue" => flags.default_on_error = tpu::cmd::copy::OnError::Warn,
            "fail" | "strict" => flags.default_on_error = tpu::cmd::copy::OnError::Fail,
            other => flags.warnings.push(format!(
                "ignoring invalid --default-on-error value: {other:?}"
            )),
        }
    } else if let Some(rest) = s.strip_prefix("--progress-detail=") {
        match rest {
            "each-file" | "each_file" => flags.progress_detail = tools::ProgressDetail::EachFile,
            "summary" => flags.progress_detail = tools::ProgressDetail::Summary,
            other => flags.warnings.push(format!(
                "ignoring invalid --progress-detail value: {other:?}"
            )),
        }
    } else if s == "--quiet" {
        flags.quiet = true;
    } else if s == "--eol-normalize" {
        flags.eol_normalize = true;
    } else if s == worker::DISABLE_ARG {
        flags.io_worker_enabled = false;
    } else if s == worker::WORKER_ARG {
        // Handled earlier (process never reaches here in worker mode);
        // ignore silently if it ever appears here.
        //
        // (Mutation testing: this arm's body is empty, so mutating the `==`
        // above to `!=` is behaviourally equivalent for every possible `s`
        // -- entering an empty branch and falling through past it produce
        // the exact same observable nothing. Not pursued.)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- request_id --------------------------------------------------------

    #[test]
    fn request_id_none_is_notification() {
        assert_eq!(request_id(None), None);
    }

    #[test]
    fn request_id_explicit_null_is_notification() {
        assert_eq!(request_id(Some(Value::Null)), None);
    }

    #[test]
    fn request_id_number_is_a_request() {
        assert_eq!(request_id(Some(Value::from(42))), Some(Value::from(42)));
    }

    #[test]
    fn request_id_string_is_a_request() {
        assert_eq!(
            request_id(Some(Value::from("abc"))),
            Some(Value::from("abc"))
        );
    }

    #[test]
    fn request_id_zero_is_a_request_not_a_notification() {
        // A falsy-but-non-null id (0) must still be treated as a request:
        // this guards against a `v.is_null()` -> `true`-style mutation that
        // would happen to also swallow every other "falsy" id.
        assert_eq!(request_id(Some(Value::from(0))), Some(Value::from(0)));
    }

    // -- send_response / send_error -----------------------------------------
    //
    // Both functions are generic over `impl io::Write`, so -- like
    // `worker::write_response` -- they can be tested directly against an
    // in-memory buffer with no stdio round trip. This closes the mutants
    // that previously only had coverage via a real subprocess exchange
    // (`tests/mcp_protocol.rs`), where a mutation to either function's body
    // silences the response entirely and the client's blocking read hangs.

    #[test]
    fn send_response_ok_emits_expected_json_line() {
        let mut buf: Vec<u8> = Vec::new();
        send_response(
            &mut buf,
            Response {
                jsonrpc: "2.0",
                id: Value::from(5),
                body: ResponseBody::Ok {
                    result: serde_json::json!({ "hello": "world" }),
                },
            },
        );
        let line = String::from_utf8(buf).unwrap();
        assert!(line.ends_with('\n'));
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 5);
        assert_eq!(parsed["result"]["hello"], "world");
        assert!(parsed.get("error").is_none());
    }

    #[test]
    fn send_response_err_emits_expected_json_line() {
        let mut buf: Vec<u8> = Vec::new();
        send_response(
            &mut buf,
            Response {
                jsonrpc: "2.0",
                id: Value::from(9),
                body: ResponseBody::Err {
                    error: RpcError {
                        code: code::METHOD_NOT_FOUND,
                        message: "nope".to_string(),
                    },
                },
            },
        );
        let line = String::from_utf8(buf).unwrap();
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["id"], 9);
        assert_eq!(parsed["error"]["code"], code::METHOD_NOT_FOUND);
        assert_eq!(parsed["error"]["message"], "nope");
        assert!(parsed.get("result").is_none());
    }

    #[test]
    fn send_error_emits_expected_json_line() {
        let mut buf: Vec<u8> = Vec::new();
        send_error(
            &mut buf,
            Value::from(3),
            code::PARSE_ERROR,
            "bad json".to_string(),
        );
        let line = String::from_utf8(buf).unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(buf_lines(&line), 1);
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 3);
        assert_eq!(parsed["error"]["code"], code::PARSE_ERROR);
        assert_eq!(parsed["error"]["message"], "bad json");
    }

    fn buf_lines(s: &str) -> usize {
        s.bytes().filter(|&b| b == b'\n').count()
    }

    // -- JSON-RPC reserved error codes ---------------------------------------
    //
    // These must be negative per the JSON-RPC 2.0 spec; pins each constant's
    // exact value against a mutation that deletes the leading `-`.

    #[test]
    fn reserved_error_codes_are_negative_and_exact() {
        assert_eq!(code::PARSE_ERROR, -32700);
        assert_eq!(code::METHOD_NOT_FOUND, -32601);
        assert_eq!(code::INVALID_PARAMS, -32602);
    }

    // -- dispatch -------------------------------------------------------------

    fn test_config() -> tools::ServerConfig {
        tools::ServerConfig::default()
    }

    #[test]
    fn dispatch_ping_returns_empty_ok_result() {
        let config = test_config();
        let worker = IoWorkerHandle::disabled();
        let mut out: Vec<u8> = Vec::new();
        let body = dispatch("ping", None, &config, &worker, &mut out);
        match body {
            ResponseBody::Ok { result } => assert_eq!(result, serde_json::json!({})),
            ResponseBody::Err { .. } => panic!("ping must not error"),
        }
    }

    #[test]
    fn dispatch_shutdown_returns_null_ok_result() {
        let config = test_config();
        let worker = IoWorkerHandle::disabled();
        let mut out: Vec<u8> = Vec::new();
        let body = dispatch("shutdown", None, &config, &worker, &mut out);
        match body {
            ResponseBody::Ok { result } => assert_eq!(result, Value::Null),
            ResponseBody::Err { .. } => panic!("shutdown must not error"),
        }
    }

    #[test]
    fn log_info_emits_expected_notification() {
        let mut buf: Vec<u8> = Vec::new();
        log_info(&mut buf, "hello");
        let line = String::from_utf8(buf).unwrap();
        assert!(line.ends_with('\n'));
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["method"], "notifications/message");
        assert_eq!(parsed["params"]["level"], "info");
        assert_eq!(parsed["params"]["logger"], "tpu-mcp");
        assert_eq!(parsed["params"]["data"], "hello");
    }

    #[test]
    fn log_warn_emits_expected_notification() {
        let mut buf: Vec<u8> = Vec::new();
        log_warn(&mut buf, "uh oh");
        let line = String::from_utf8(buf).unwrap();
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["params"]["level"], "warning");
        assert_eq!(parsed["params"]["logger"], "tpu-mcp");
        assert_eq!(parsed["params"]["data"], "uh oh");
    }

    #[test]
    fn dispatch_unknown_method_is_method_not_found() {
        let config = test_config();
        let worker = IoWorkerHandle::disabled();
        let mut out: Vec<u8> = Vec::new();
        let body = dispatch("nonexistent/method", None, &config, &worker, &mut out);
        match body {
            ResponseBody::Err { error } => assert_eq!(error.code, code::METHOD_NOT_FOUND),
            ResponseBody::Ok { .. } => panic!("unknown method must error"),
        }
    }

    // -- apply_arg ------------------------------------------------------------

    fn default_flags() -> ArgFlags {
        ArgFlags {
            verify_delay_ms: 100,
            default_on_error: tpu::cmd::copy::OnError::Warn,
            progress_detail: tools::ProgressDetail::EachFile,
            quiet: false,
            eol_normalize: false,
            io_worker_enabled: true,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn apply_arg_verify_delay_ms_valid() {
        let mut flags = default_flags();
        apply_arg(&mut flags, "--verify-delay-ms=250");
        assert_eq!(flags.verify_delay_ms, 250);
        assert!(flags.warnings.is_empty());
    }

    #[test]
    fn apply_arg_verify_delay_ms_invalid_warns_and_keeps_default() {
        let mut flags = default_flags();
        apply_arg(&mut flags, "--verify-delay-ms=notanumber");
        assert_eq!(flags.verify_delay_ms, 100);
        assert_eq!(flags.warnings.len(), 1);
    }

    #[test]
    fn apply_arg_default_on_error_each_recognised_value() {
        for (arg, expected) in [
            ("warn", tpu::cmd::copy::OnError::Warn),
            ("continue", tpu::cmd::copy::OnError::Warn),
            ("fail", tpu::cmd::copy::OnError::Fail),
            ("strict", tpu::cmd::copy::OnError::Fail),
        ] {
            let mut flags = default_flags();
            apply_arg(&mut flags, &format!("--default-on-error={arg}"));
            assert_eq!(flags.default_on_error, expected, "arg={arg}");
            assert!(flags.warnings.is_empty(), "arg={arg}");
        }
    }

    #[test]
    fn apply_arg_default_on_error_invalid_warns() {
        let mut flags = default_flags();
        apply_arg(&mut flags, "--default-on-error=bogus");
        assert_eq!(flags.warnings.len(), 1);
    }

    #[test]
    fn apply_arg_progress_detail_each_recognised_value() {
        for (arg, expected) in [
            ("each-file", tools::ProgressDetail::EachFile),
            ("each_file", tools::ProgressDetail::EachFile),
            ("summary", tools::ProgressDetail::Summary),
        ] {
            let mut flags = default_flags();
            apply_arg(&mut flags, &format!("--progress-detail={arg}"));
            assert_eq!(flags.progress_detail, expected, "arg={arg}");
            assert!(flags.warnings.is_empty(), "arg={arg}");
        }
    }

    #[test]
    fn apply_arg_progress_detail_invalid_warns() {
        let mut flags = default_flags();
        apply_arg(&mut flags, "--progress-detail=bogus");
        assert_eq!(flags.warnings.len(), 1);
    }

    #[test]
    fn apply_arg_quiet_sets_flag() {
        let mut flags = default_flags();
        apply_arg(&mut flags, "--quiet");
        assert!(flags.quiet);
    }

    #[test]
    fn apply_arg_eol_normalize_sets_flag() {
        let mut flags = default_flags();
        apply_arg(&mut flags, "--eol-normalize");
        assert!(flags.eol_normalize);
    }

    #[test]
    fn apply_arg_disable_worker_arg_clears_flag() {
        let mut flags = default_flags();
        apply_arg(&mut flags, worker::DISABLE_ARG);
        assert!(!flags.io_worker_enabled);
    }

    #[test]
    fn apply_arg_worker_sentinel_arg_is_a_no_op() {
        let mut flags = default_flags();
        apply_arg(&mut flags, worker::WORKER_ARG);
        assert!(flags.io_worker_enabled);
        assert!(flags.warnings.is_empty());
    }

    #[test]
    fn apply_arg_unrecognised_arg_is_silently_ignored() {
        let mut flags = default_flags();
        apply_arg(&mut flags, "--totally-unknown-flag");
        assert!(flags.warnings.is_empty());
        assert_eq!(flags.verify_delay_ms, 100);
    }

    // -- parse_config env-var-driven defaults --------------------------------
    //
    // `parse_config` reads real process env vars, which are global mutable
    // state shared by every thread in the process. nextest runs each test in
    // its own OS process, so that alone would make this safe -- but plain
    // `cargo test` runs tests from one binary multithreaded in a *single*
    // shared process by default, where two of these tests setting the same
    // env var concurrently would race. `ENV_VAR_TEST_LOCK` serialises just
    // this group of tests against each other so they are correct under
    // either test runner, not only under nextest.
    static ENV_VAR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn parse_config_default_on_error_env_var_recognised_values() {
        let _guard = ENV_VAR_TEST_LOCK.lock().unwrap();
        for (val, expected) in [
            ("strict", tpu::cmd::copy::OnError::Fail),
            ("fail", tpu::cmd::copy::OnError::Fail),
            ("continue", tpu::cmd::copy::OnError::Warn),
            ("warn", tpu::cmd::copy::OnError::Warn),
        ] {
            unsafe {
                std::env::set_var("TPU_DEFAULT_ERROR_MODE", val);
            }
            let (config, warnings) = parse_config();
            assert_eq!(config.default_on_error, expected, "val={val}");
            assert!(warnings.is_empty(), "val={val}");
        }
        unsafe {
            std::env::remove_var("TPU_DEFAULT_ERROR_MODE");
        }
    }

    #[test]
    fn parse_config_default_on_error_env_var_unrecognised_nonempty_warns() {
        let _guard = ENV_VAR_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("TPU_DEFAULT_ERROR_MODE", "bogus");
        }
        let (config, warnings) = parse_config();
        assert_eq!(config.default_on_error, tpu::cmd::copy::OnError::Warn);
        assert_eq!(warnings.len(), 1);
        unsafe {
            std::env::remove_var("TPU_DEFAULT_ERROR_MODE");
        }
    }

    /// An empty (but present) env var value must fall back to the default
    /// silently -- no warning -- pinning the `!other.is_empty()` guard
    /// against a mutation that would make an empty value warn too.
    #[test]
    fn parse_config_default_on_error_env_var_empty_is_silent_default() {
        let _guard = ENV_VAR_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("TPU_DEFAULT_ERROR_MODE", "");
        }
        let (config, warnings) = parse_config();
        assert_eq!(config.default_on_error, tpu::cmd::copy::OnError::Warn);
        assert!(warnings.is_empty(), "empty value must not warn");
        unsafe {
            std::env::remove_var("TPU_DEFAULT_ERROR_MODE");
        }
    }

    #[test]
    fn parse_config_progress_detail_env_var_recognised_values() {
        let _guard = ENV_VAR_TEST_LOCK.lock().unwrap();
        for (val, expected) in [
            ("summary", tools::ProgressDetail::Summary),
            ("each-file", tools::ProgressDetail::EachFile),
            ("each_file", tools::ProgressDetail::EachFile),
        ] {
            unsafe {
                std::env::set_var("TPU_PROGRESS_DETAIL", val);
            }
            let (config, warnings) = parse_config();
            assert_eq!(config.progress_detail, expected, "val={val}");
            assert!(warnings.is_empty(), "val={val}");
        }
        unsafe {
            std::env::remove_var("TPU_PROGRESS_DETAIL");
        }
    }

    #[test]
    fn parse_config_progress_detail_env_var_unrecognised_nonempty_warns() {
        let _guard = ENV_VAR_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("TPU_PROGRESS_DETAIL", "bogus");
        }
        let (config, warnings) = parse_config();
        assert_eq!(config.progress_detail, tools::ProgressDetail::EachFile);
        assert_eq!(warnings.len(), 1);
        unsafe {
            std::env::remove_var("TPU_PROGRESS_DETAIL");
        }
    }

    #[test]
    fn parse_config_progress_detail_env_var_empty_is_silent_default() {
        let _guard = ENV_VAR_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("TPU_PROGRESS_DETAIL", "");
        }
        let (config, warnings) = parse_config();
        assert_eq!(config.progress_detail, tools::ProgressDetail::EachFile);
        assert!(warnings.is_empty(), "empty value must not warn");
        unsafe {
            std::env::remove_var("TPU_PROGRESS_DETAIL");
        }
    }

    /// `TPU_MCP_QUIET` set to a non-empty value must enable quiet mode (and
    /// therefore disable `trace`); unset or empty must leave tracing on --
    /// pins both the `!v.is_empty()` guard and the `trace: !flags.quiet`
    /// inversion in one round trip.
    #[test]
    fn parse_config_quiet_env_var_non_empty_disables_trace() {
        let _guard = ENV_VAR_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("TPU_MCP_QUIET", "1");
        }
        let (config, _) = parse_config();
        assert!(!config.trace, "non-empty TPU_MCP_QUIET must disable trace");
        unsafe {
            std::env::remove_var("TPU_MCP_QUIET");
        }
    }

    #[test]
    fn parse_config_quiet_env_var_empty_or_unset_keeps_trace_on() {
        let _guard = ENV_VAR_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("TPU_MCP_QUIET");
        }
        let (config, _) = parse_config();
        assert!(config.trace, "unset TPU_MCP_QUIET must leave trace on");

        unsafe {
            std::env::set_var("TPU_MCP_QUIET", "");
        }
        let (config, _) = parse_config();
        assert!(config.trace, "empty TPU_MCP_QUIET must leave trace on");
        unsafe {
            std::env::remove_var("TPU_MCP_QUIET");
        }
    }

    /// `TPU_MCP_NO_IO_WORKER` set to a non-empty value must disable the
    /// worker subsystem on Windows -- pins the `&&` in
    /// `cfg!(windows) && ...is_none_or(...)` against an `||` mutation, which
    /// would make `io_worker_enabled` always `true` regardless of this env
    /// var (since `cfg!(windows)` is `true` on this build).
    #[test]
    #[cfg(windows)]
    fn parse_config_no_io_worker_env_var_non_empty_disables_worker() {
        let _guard = ENV_VAR_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("TPU_MCP_NO_IO_WORKER", "1");
        }
        let (config, _) = parse_config();
        assert!(
            !config.io_worker_enabled,
            "non-empty TPU_MCP_NO_IO_WORKER must disable the io worker on Windows"
        );
        unsafe {
            std::env::remove_var("TPU_MCP_NO_IO_WORKER");
        }
    }

    #[test]
    #[cfg(windows)]
    fn parse_config_no_io_worker_env_var_empty_or_unset_keeps_worker_enabled() {
        let _guard = ENV_VAR_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("TPU_MCP_NO_IO_WORKER");
        }
        let (config, _) = parse_config();
        assert!(config.io_worker_enabled);
        unsafe {
            std::env::remove_var("TPU_MCP_NO_IO_WORKER");
        }
    }

    /// `TPU_EOL_NORMALIZE` set to a non-empty value must enable
    /// `eol_normalize`; unset or empty must leave it off -- pins the
    /// `!v.is_empty()` guard.
    #[test]
    fn parse_config_eol_normalize_env_var_non_empty_enables_flag() {
        let _guard = ENV_VAR_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("TPU_EOL_NORMALIZE", "1");
        }
        let (config, _) = parse_config();
        assert!(config.eol_normalize);
        unsafe {
            std::env::remove_var("TPU_EOL_NORMALIZE");
        }
    }

    #[test]
    fn parse_config_eol_normalize_env_var_empty_or_unset_leaves_flag_off() {
        let _guard = ENV_VAR_TEST_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("TPU_EOL_NORMALIZE");
        }
        let (config, _) = parse_config();
        assert!(!config.eol_normalize);

        unsafe {
            std::env::set_var("TPU_EOL_NORMALIZE", "");
        }
        let (config, _) = parse_config();
        assert!(!config.eol_normalize);
        unsafe {
            std::env::remove_var("TPU_EOL_NORMALIZE");
        }
    }
}
