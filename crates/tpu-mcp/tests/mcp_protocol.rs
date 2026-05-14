// Copyright (c) 2026, Michael Grier

//! MCP protocol-level integration tests for tpu-mcp.
//!
//! These tests spawn the actual `tpu-mcp` binary and communicate via
//! JSON-RPC 2.0 over stdin/stdout — the same protocol VS Code uses.
//! They exercise the complete stack: JSON-RPC dispatch → tools.rs → tpu library,
//! and are the Rust equivalent of the PowerShell smoke test in `scratch/`.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
};

use serde_json::{json, Value};

/// Locate the tpu-mcp binary for the current build profile.
///
/// Cargo sets `CARGO_BIN_EXE_tpu_mcp` at test runtime; if that is absent
/// (older toolchains) we fall back to locating the binary next to the test
/// executable in target/{profile}/.
fn bin_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_tpu_mcp") {
        return std::path::PathBuf::from(p);
    }
    // Fallback: test binary is in target/{profile}/deps/; the built binary
    // lives one directory up in target/{profile}/.
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // strip the test-binary filename
    if p.file_name().is_some_and(|n| n == "deps") {
        p.pop(); // step up from deps/ to target/{profile}/
    }
    let name = if cfg!(windows) {
        "tpu-mcp.exe"
    } else {
        "tpu-mcp"
    };
    p.push(name);
    p
}

// --- MCP session helper -------------------------------------------------------

struct McpSession {
    child: Child,
    stdin: std::io::BufWriter<std::process::ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    seq: u64,
}

impl McpSession {
    fn start() -> Self {
        let mut child = Command::new(bin_path())
            .arg("--verify-delay-ms=0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn tpu-mcp binary");

        let stdin = std::io::BufWriter::new(child.stdin.take().unwrap());
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            seq: 0,
        }
    }

    fn next_id(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn send_raw(&mut self, msg: Value) {
        let mut line = serde_json::to_string(&msg).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn recv_raw(&mut self) -> Value {
        // Skip server-initiated notifications (no `id`); we only return when
        // we see a response message. tpu-mcp emits `notifications/message`
        // log lines for the startup banner and per-request dispatch trace.
        loop {
            let mut line = String::new();
            self.stdout
                .read_line(&mut line)
                .expect("read from tpu-mcp stdout");
            assert!(!line.is_empty(), "tpu-mcp closed stdout unexpectedly");
            let v: Value =
                serde_json::from_str(line.trim()).expect("tpu-mcp response is not valid JSON");
            if v.get("id").is_some() {
                return v;
            }
            // else: notification \u2014 keep reading.
        }
    }

    /// Send a request and wait for its response.  Panics on RPC error.
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id();
        self.send_raw(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        let resp = self.recv_raw();
        if let Some(err) = resp.get("error")
            && !err.is_null() {
                panic!("RPC error for '{method}': {err}");
            }
        resp["result"].clone()
    }

    /// Send a notification (fire-and-forget; no response is expected).
    fn notify(&mut self, method: &str) {
        self.send_raw(json!({ "jsonrpc": "2.0", "method": method, "params": {} }));
    }

    /// Perform the MCP initialize handshake.
    fn initialize(&mut self) {
        let result = self.rpc(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "mcp_protocol_test", "version": "1.0" },
            }),
        );
        assert!(
            result.to_string().contains("tpu-mcp"),
            "server name must include 'tpu-mcp'; got: {result}",
        );
        // notifications/initialized is fire-and-forget — no response expected.
        self.notify("notifications/initialized");
    }

    /// Call a tool and return the concatenated text content from the response.
    fn call_tool(&mut self, tool: &str, args: Value) -> String {
        let result = self.rpc("tools/call", json!({ "name": tool, "arguments": args }));
        match result["content"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
            None => String::new(),
        }
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- assertion helpers --------------------------------------------------------

fn assert_has(label: &str, haystack: &str, needle: &str) {
    assert!(
        haystack.contains(needle),
        "FAIL [{label}]: expected {needle:?} in:\n{haystack}"
    );
}

fn assert_lacks(label: &str, haystack: &str, needle: &str) {
    assert!(
        !haystack.contains(needle),
        "FAIL [{label}]: did NOT expect {needle:?} in:\n{haystack}"
    );
}

// --- tests -------------------------------------------------------------------

/// MCP-IT-1: initialize handshake succeeds; tools/list includes all required tools.
#[test]
fn mcp_it_1_initialize_and_tools_list() {
    let mut s = McpSession::start();
    s.initialize();

    let list = s.rpc("tools/list", json!({}));
    let names: Vec<&str> = list["tools"]
        .as_array()
        .expect("tools must be an array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();

    for expected in [
        "tpu_read_file",
        "tpu_write_file",
        "tpu_replace_in_file",
        "tpu_edit_file",
        "tpu_read_file_binary",
        "tpu_read_file_escaped",
        "tpu_validate_file",
        "tpu_read_head",
        "tpu_read_tail",
        "tpu_count_file",
        "tpu_append_file",
        "tpu_find",
        "tpu_copy_file",
        "tpu_render_file",
        "tpu_setup",
        "tpu_stat_file",
    ] {
        assert!(
            names.contains(&expected),
            "tools/list must include '{expected}'; got: {names:?}"
        );
    }
}

/// MCP-IT-2: tpu_write_file creates a file; response contains mtime+size stamp.
/// tpu_read_file returns the correct content.
#[test]
fn mcp_it_2_write_then_read() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("write_read.txt");
    let path = f.to_str().unwrap();

    let mut s = McpSession::start();
    s.initialize();

    let out = s.call_tool(
        "tpu_write_file",
        json!({
            "file": path,
            "content": "hello world\nline two\nline three\n",
        }),
    );
    assert_has("write stamp mtime", &out, "[mtime=");
    assert_has("write stamp size", &out, "size=");

    let content = s.call_tool("tpu_read_file", json!({ "file": path }));
    assert_has("read line 1", &content, "hello world");
    assert_has("read line 2", &content, "line two");
    assert_has("read line 3", &content, "line three");
}

/// MCP-IT-3: tpu_replace_in_file replaces text and includes a write stamp.
#[test]
fn mcp_it_3_replace_basic() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("replace_basic.txt");
    std::fs::write(&f, "hello world\n").unwrap();

    let mut s = McpSession::start();
    s.initialize();

    let out = s.call_tool(
        "tpu_replace_in_file",
        json!({
            "file": f.to_str().unwrap(),
            "pattern": "world",
            "replacement": "earth",
            "fixed_strings": true,
        }),
    );
    assert_has("replace stamp", &out, "[mtime=");

    let content = std::fs::read_to_string(&f).unwrap();
    assert_has("word replaced", &content, "earth");
    assert_lacks("old word gone", &content, "world");
}

/// MCP-IT-4: `\n` in a tpu_replace_in_file replacement string expands to a real
/// newline rather than the two-character sequence backslash-n.
///
/// This validates the escape-expansion feature (RE milestone).
#[test]
fn mcp_it_4_replace_backslash_n_expands_to_newline() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("replace_escape.txt");
    std::fs::write(&f, "line two\n").unwrap();

    let mut s = McpSession::start();
    s.initialize();

    // The Rust string "second\\nthird injected" serialises to the JSON string
    // "second\nthird injected" (backslash + n, not a newline character).
    // tpu-mcp must expand that \n to a real newline before applying the regex.
    s.call_tool(
        "tpu_replace_in_file",
        json!({
            "file": f.to_str().unwrap(),
            "pattern": "line two",
            "replacement": "second\\nthird injected",
            "fixed_strings": true,
        }),
    );

    let content = std::fs::read_to_string(&f).unwrap();
    assert_has("second present", &content, "second");
    assert_has("third present", &content, "third injected");
    assert_lacks("no literal \\n", &content, "second\\nthird");

    // The two injected words must be on separate lines.
    let lines: Vec<&str> = content.lines().collect();
    assert!(
        lines.iter().any(|l| l.trim() == "second"),
        "'second' must be on its own line; content: {content:?}"
    );
    assert!(
        lines.iter().any(|l| l.trim() == "third injected"),
        "'third injected' must be on its own line; content: {content:?}"
    );
}

/// MCP-IT-5: tpu_append_file appends content and includes a stamp.
/// tpu_count_file returns a summary that mentions lines.
#[test]
fn mcp_it_5_append_and_count() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("append_count.txt");
    std::fs::write(&f, "line1\n").unwrap();

    let mut s = McpSession::start();
    s.initialize();

    let out = s.call_tool(
        "tpu_append_file",
        json!({
            "file": f.to_str().unwrap(),
            "content": "appended line\n",
        }),
    );
    assert_has("append stamp", &out, "[mtime=");

    let content = std::fs::read_to_string(&f).unwrap();
    assert_has("appended text on disk", &content, "appended line");

    let count_out = s.call_tool("tpu_count_file", json!({ "file": f.to_str().unwrap() }));
    assert_has("count mentions lines", &count_out, "lines");
}

/// MCP-IT-6: tpu_stat_file returns valid JSON with size, mtime_epoch_ms, and
/// readonly fields.  The mtime from tpu_write_file agrees within 2 seconds.
#[test]
fn mcp_it_6_stat_file_and_stamp_consistency() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("stat_check.txt");
    let path = f.to_str().unwrap();

    let mut s = McpSession::start();
    s.initialize();

    let write_out = s.call_tool(
        "tpu_write_file",
        json!({
            "file": path,
            "content": "stat test\n",
        }),
    );

    // Parse mtime from "... [mtime=NNN, size=NNN]".
    let mtime_write: u64 = write_out
        .split("mtime=")
        .nth(1)
        .and_then(|s| s.split([',', ']', ' ']).next())
        .and_then(|s| s.parse().ok())
        .expect("write response must contain parseable mtime=N");

    let stat_out = s.call_tool("tpu_stat_file", json!({ "file": path }));
    let stat: Value = serde_json::from_str(&stat_out)
        .unwrap_or_else(|_| panic!("tpu_stat_file must return valid JSON; got: {stat_out:?}"));

    let mtime_stat = stat["mtime_epoch_ms"]
        .as_u64()
        .expect("stat must have mtime_epoch_ms");
    assert!(
        stat["size"].as_u64().unwrap_or(0) > 0,
        "stat size must be >0"
    );
    assert_eq!(stat["readonly"], false, "new file must not be readonly");
    assert!(
        mtime_write.abs_diff(mtime_stat) < 2_000,
        "write mtime {mtime_write} and stat mtime {mtime_stat} must agree within 2 s"
    );
}

/// MCP-IT-7: tpu_find returns matching lines; returns empty string when no match.
#[test]
fn mcp_it_7_find_matches_and_no_match() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("find_target.txt");
    std::fs::write(&f, "alpha fox here\nbeta bar there\ngamma fox again\n").unwrap();

    let mut s = McpSession::start();
    s.initialize();

    let out = s.call_tool(
        "tpu_find",
        json!({
            "pattern": "fox",
            "path": f.to_str().unwrap(),
        }),
    );
    assert_has("find hit 1", &out, "alpha fox here");
    assert_has("find hit 2", &out, "gamma fox again");
    assert_lacks("non-matching line excluded", &out, "beta bar there");

    let no_match = s.call_tool(
        "tpu_find",
        json!({
            "pattern": "zzz_no_match",
            "path": f.to_str().unwrap(),
        }),
    );
    assert!(
        no_match.trim().is_empty(),
        "no-match result must be empty; got: {no_match:?}"
    );
}

/// MCP-IT-8: `tpu_copy_file` copies a file; result JSON includes counts.
/// Overwriting an existing destination with `overwrite=true` replaces it.
#[test]
fn mcp_it_8_copy_file_basic_and_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.txt");
    let dst = dir.path().join("dst.txt");
    std::fs::write(&src, "copy me\n").unwrap();

    let mut s = McpSession::start();
    s.initialize();

    // First copy — dst does not exist yet.
    let out = s.call_tool(
        "tpu_copy_file",
        json!({
            "source":    src.to_str().unwrap(),
            "dest":      dst.to_str().unwrap(),
            "overwrite": false,
        }),
    );
    let v: serde_json::Value = serde_json::from_str(&out)
        .unwrap_or_else(|e| panic!("tpu_copy_file result must be JSON; got {out:?}: {e}"));
    assert_eq!(v["copied"], 1, "copied count must be 1; result: {v}");
    assert_eq!(v["skipped"], 0, "skipped count must be 0; result: {v}");
    assert_eq!(
        std::fs::read_to_string(&dst).unwrap(),
        "copy me\n",
        "destination content must match source"
    );

    // Overwrite — dst already exists; overwrite=true must replace it atomically.
    std::fs::write(&src, "updated content\n").unwrap();
    let out2 = s.call_tool(
        "tpu_copy_file",
        json!({
            "source":    src.to_str().unwrap(),
            "dest":      dst.to_str().unwrap(),
            "overwrite": true,
        }),
    );
    let v2: serde_json::Value = serde_json::from_str(&out2)
        .unwrap_or_else(|e| panic!("tpu_copy_file overwrite result must be JSON; got {out2:?}: {e}"));
    assert_eq!(v2["copied"], 1, "overwrite copied count must be 1; result: {v2}");
    assert_eq!(
        std::fs::read_to_string(&dst).unwrap(),
        "updated content\n",
        "destination must contain updated source after overwrite"
    );

    // Skip — dst exists and overwrite=false: skipped=1, copied=0.
    let out3 = s.call_tool(
        "tpu_copy_file",
        json!({
            "source":    src.to_str().unwrap(),
            "dest":      dst.to_str().unwrap(),
            "overwrite": false,
        }),
    );
    let v3: serde_json::Value = serde_json::from_str(&out3)
        .unwrap_or_else(|e| panic!("tpu_copy_file skip result must be JSON; got {out3:?}: {e}"));
    assert_eq!(v3["skipped"], 1, "skip count must be 1; result: {v3}");
    assert_eq!(v3["copied"], 0, "copied count must be 0 when skipped; result: {v3}");
}

/// MCP-IT-9: `tpu_render_file` substitutes tokens and writes the output file.
/// Also verifies that an empty token key is rejected.
#[test]
fn mcp_it_9_render_file_substitution_and_empty_key_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.txt");

    let mut s = McpSession::start();
    s.initialize();

    // Basic substitution — inline template.
    let result = s.call_tool(
        "tpu_render_file",
        json!({
            "template": "Hello {{NAME}}, you are {{AGE}} years old.",
            "output":   out.to_str().unwrap(),
            "vars": { "NAME": "Alice", "AGE": "30" },
        }),
    );
    let v: serde_json::Value = serde_json::from_str(&result)
        .unwrap_or_else(|e| panic!("tpu_render_file result must be JSON; got {result:?}: {e}"));
    assert_eq!(v["substitutions"], 2, "substitution count must be 2; result: {v}");
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "Hello Alice, you are 30 years old.",
        "rendered output must match expected"
    );

    // Empty key must be rejected with an error (isError: true in the MCP response).
    let id = s.next_id();
    s.send_raw(json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": "tpu_render_file",
            "arguments": {
                "template": "{{}}",
                "output":   out.to_str().unwrap(),
                "vars": { "": "value" },
            }
        },
    }));
    let resp = s.recv_raw();
    // MCP errors can be either a JSON-RPC error or a tool result with isError=true.
    let is_error = resp.get("error").is_some()
        || resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(is_error, "empty var key must produce an error; got: {resp}");
}

/// MCP-IT-10: `tpu_setup` without a `target` returns the canonical Markdown
/// block as plain text (not JSON).
#[test]
fn mcp_it_10_setup_returns_markdown_block() {
    let mut s = McpSession::start();
    s.initialize();

    let out = s.call_tool("tpu_setup", json!({}));
    // The block must contain the tpu-mcp:setup markers and at least one table row.
    assert_has("setup begin marker", &out, "tpu-mcp:setup:begin");
    assert_has("setup end marker",   &out, "tpu-mcp:setup:end");
    assert_has("tpu_read_file row",  &out, "tpu_read_file");
    // Plain text — must NOT look like a top-level JSON object.
    assert!(
        !out.trim_start().starts_with('{'),
        "tpu_setup without target must return plain Markdown, not JSON; got: {out:?}"
    );
}
