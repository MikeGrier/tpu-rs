// Copyright (c) 2026, Michael Grier

#![cfg(windows)]

//! Chaos tests for the io-worker subsystem.
//!
//! These spawn the real `tpu-mcp` binary, drive it through MCP JSON-RPC,
//! and forcibly terminate its io-worker child process under load.  They
//! verify that:
//!
//! - every tool call still succeeds (either via worker retry or via the
//!   in-process fallback);
//! - the final on-disk file state is correct (whole-document idempotency
//!   guarantee that justifies transparent retry);
//! - worker turbulence is surfaced to the MCP client as
//!   `notifications/message` warnings, not silently swallowed.
//!
//! Windows-only because the io worker is on by default only on Windows;
//! on other platforms `tpu-mcp` runs every call in-process and these
//! tests would have nothing to exercise.

use std::{
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use serde_json::{Value, json};
use sysinfo::{Pid, System};

// ── helpers ──────────────────────────────────────────────────────────────────

fn bin_path() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_tpu_mcp") {
        return PathBuf::from(p);
    }
    // Fallback: test binary is in target/{profile}/deps/; the built binary
    // lives one directory up in target/{profile}/.
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    if p.file_name().is_some_and(|n| n == "deps") {
        p.pop();
    }
    p.push("tpu-mcp.exe");
    p
}

/// All running children of `parent_pid` (i.e. the io-worker, if any).
fn worker_pids_for(parent_pid: u32) -> Vec<u32> {
    let sys = System::new_all();
    let parent = Pid::from_u32(parent_pid);
    sys.processes()
        .values()
        .filter(|p| p.parent() == Some(parent))
        .map(|p| p.pid().as_u32())
        .collect()
}

/// Force-kill `pid`. Returns true if the kill syscall reported success.
fn kill_pid(pid: u32) -> bool {
    let sys = System::new_all();
    sys.process(Pid::from_u32(pid))
        .map(|p| p.kill())
        .unwrap_or(false)
}

/// Kill every worker child of `parent_pid`. Returns the number killed.
fn kill_all_workers_of(parent_pid: u32) -> usize {
    let mut killed = 0;
    for pid in worker_pids_for(parent_pid) {
        if kill_pid(pid) {
            killed += 1;
        }
    }
    killed
}

/// Extract the `data` field of an MCP `notifications/message` payload.
fn notif_text(n: &Value) -> Option<&str> {
    n.get("params")?.get("data")?.as_str()
}

// ── MCP session ──────────────────────────────────────────────────────────────

/// A minimal MCP-over-stdio session that collects unsolicited
/// notifications (which is how `tpu-mcp` reports worker turbulence) into
/// a buffer the test can later inspect.
struct ChaosSession {
    child: Option<Child>,
    stdin: BufWriter<std::process::ChildStdin>,
    stdout: BufReader<std::process::ChildStdout>,
    seq: u64,
    notifications: Vec<Value>,
}

impl ChaosSession {
    fn start() -> Self {
        let mut child = Command::new(bin_path())
            .arg("--verify-delay-ms=0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn tpu-mcp");
        let stdin = BufWriter::new(child.stdin.take().expect("piped stdin"));
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut s = Self {
            child: Some(child),
            stdin,
            stdout,
            seq: 1,
            notifications: Vec::new(),
        };
        // Drain the startup `notifications/message` lines (banner, tool
        // list, "io worker enabled" advisory) by issuing the standard
        // initialize handshake and waiting for its response.
        let _ = s.request("initialize", json!({}));
        s
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().expect("child not yet reaped").id()
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.seq;
        self.seq += 1;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_string(&req).expect("encode request");
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).expect("write request");
        self.stdin.flush().expect("flush request");

        loop {
            let mut buf = String::new();
            let n = self.stdout.read_line(&mut buf).expect("read line");
            assert!(n > 0, "unexpected EOF from tpu-mcp");
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(trimmed).unwrap_or_else(|e| {
                panic!("bad JSON from tpu-mcp: {e}; raw: {trimmed:?}");
            });
            if v.get("id").and_then(|x| x.as_u64()) == Some(id) {
                return v;
            }
            // Unsolicited message (almost always a notifications/message).
            self.notifications.push(v);
        }
    }

    fn tool_call(&mut self, name: &str, args: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": args}))
    }

    /// Assert that the tool call returned a non-error result and return
    /// the full JSON-RPC response for further inspection.
    fn assert_tool_success(&mut self, name: &str, args: Value) -> Value {
        let resp = self.tool_call(name, args);
        let result = resp
            .get("result")
            .unwrap_or_else(|| panic!("tool '{name}' had no result: {resp}"));
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(!is_error, "tool '{name}' returned isError=true: {resp}");
        resp
    }

    fn take_notifications(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.notifications)
    }

    /// Force the io worker into existence (it spawns lazily on first
    /// call).  Subsequent kills then have something to terminate.
    fn warmup(&mut self, tmp: &Path) {
        let f = tmp.join("warmup.txt");
        std::fs::write(&f, "warm").expect("seed warmup file");
        self.assert_tool_success("tpu_read_file", json!({"file": f.to_string_lossy()}));
        // Briefly wait for the worker to be visible in the process table.
        let parent = self.pid();
        for _ in 0..20 {
            if !worker_pids_for(parent).is_empty() {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        // It's OK if the worker isn't observable here — the very first
        // call might race against process-table visibility on slow CI.
        // The actual tests don't depend on warmup observability.
    }
}

impl Drop for ChaosSession {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Snapshot worker pids before reaping the parent — once the
            // parent goes the parent-pid linkage is gone too.
            let leftover = worker_pids_for(child.id());
            let _ = child.kill();
            let _ = child.wait();
            for pid in leftover {
                let _ = kill_pid(pid);
            }
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

/// Kill the io worker between every few writes; verify every write still
/// succeeds, the file contents are correct on read-back, and at least one
/// `io worker died` warning notification was surfaced via MCP.
#[test]
fn chaos_kill_between_writes_all_succeed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut s = ChaosSession::start();
    s.warmup(tmp.path());
    let parent = s.pid();

    let total: usize = 24;
    let kill_every: usize = 3;
    let mut total_kills = 0;

    for i in 0..total {
        if i > 0 && i % kill_every == 0 {
            total_kills += kill_all_workers_of(parent);
            // Brief sleep so Windows actually delivers the termination
            // before the next request goes out (otherwise the parent
            // would happily write into a still-open pipe buffer and only
            // notice the death on the read side).
            thread::sleep(Duration::from_millis(50));
        }

        let path = tmp.path().join(format!("file_{i:03}.txt"));
        let content = format!("content for {i}\n");
        s.assert_tool_success(
            "tpu_write_file",
            json!({"file": path.to_string_lossy(), "content": content}),
        );

        // Read back via tpu-mcp (exercises the worker path again) and
        // also via the filesystem (truth check independent of tpu-mcp).
        let read = s.tool_call("tpu_read_file", json!({"file": path.to_string_lossy()}));
        let text = read
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or_else(|| panic!("read {i} missing text: {read}"));
        assert_eq!(text, content, "tpu_read_file content mismatch for file {i}");

        let on_disk = std::fs::read_to_string(&path).expect("read back from disk");
        assert_eq!(on_disk, content, "on-disk content mismatch for file {i}");
    }

    assert!(total_kills > 0, "no workers were killed; test is a no-op");

    let notes = s.take_notifications();
    let died: Vec<&str> = notes
        .iter()
        .filter_map(notif_text)
        .filter(|t| t.contains("io worker died"))
        .collect();
    assert!(
        !died.is_empty(),
        "expected at least one 'io worker died' notification after {total_kills} kills; all notifications: {notes:#?}",
    );
}

/// A background killer thread terminates any worker it finds for the
/// entire duration of the test.  All writes must still succeed (whether
/// via successful retry or via in-process fallback) and final on-disk
/// contents must match every request.
#[test]
fn chaos_concurrent_killer_thread() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut s = ChaosSession::start();
    s.warmup(tmp.path());
    let parent = s.pid();

    let stop = Arc::new(AtomicBool::new(false));
    let killer = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut kills = 0usize;
            while !stop.load(Ordering::Relaxed) {
                kills += kill_all_workers_of(parent);
                thread::sleep(Duration::from_millis(75));
            }
            kills
        })
    };

    let total: usize = 40;
    for i in 0..total {
        let path = tmp.path().join(format!("c_{i:03}.txt"));
        let content = format!("c{i}\n");
        s.assert_tool_success(
            "tpu_write_file",
            json!({"file": path.to_string_lossy(), "content": content}),
        );
    }

    stop.store(true, Ordering::Relaxed);
    let kills = killer.join().expect("killer thread");
    assert!(
        kills > 0,
        "killer thread should have killed at least one worker"
    );

    for i in 0..total {
        let path = tmp.path().join(format!("c_{i:03}.txt"));
        let got = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(got, format!("c{i}\n"), "file {i} contents mismatch");
    }
}

/// Repeatedly replace the same file while a background killer thread
/// terminates the worker.  The final on-disk content must equal the
/// content of the last request — the whole-document idempotency
/// invariant the design relies on for safe retry.
#[test]
fn chaos_idempotent_repeated_writes_to_same_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut s = ChaosSession::start();
    s.warmup(tmp.path());
    let parent = s.pid();
    let target = tmp.path().join("idem.txt");

    let stop = Arc::new(AtomicBool::new(false));
    let killer = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = kill_all_workers_of(parent);
                thread::sleep(Duration::from_millis(60));
            }
        })
    };

    let total: usize = 30;
    for i in 0..total {
        let content = format!("revision {i}\n");
        s.assert_tool_success(
            "tpu_write_file",
            json!({"file": target.to_string_lossy(), "content": content}),
        );
    }

    stop.store(true, Ordering::Relaxed);
    let _ = killer.join();

    let got = std::fs::read_to_string(&target).expect("read back final state");
    assert_eq!(
        got,
        format!("revision {}\n", total - 1),
        "final file state does not match last write"
    );
}

/// Mixed-operation chaos: interleave writes, replaces, appends, and
/// reads against a small set of files while a background killer thread
/// thrashes the worker.  All operations must succeed and the final
/// concatenation of the per-file content invariant must hold.
#[test]
fn chaos_mixed_operations_under_killer() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut s = ChaosSession::start();
    s.warmup(tmp.path());
    let parent = s.pid();

    let files: Vec<PathBuf> = (0..4)
        .map(|i| tmp.path().join(format!("mix_{i}.txt")))
        .collect();
    // Seed each file so that read / replace / append have something to
    // operate on.
    for (i, f) in files.iter().enumerate() {
        s.assert_tool_success(
            "tpu_write_file",
            json!({"file": f.to_string_lossy(), "content": format!("seed-{i}\n")}),
        );
    }

    let stop = Arc::new(AtomicBool::new(false));
    let killer = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = kill_all_workers_of(parent);
                thread::sleep(Duration::from_millis(50));
            }
        })
    };

    let rounds: usize = 12;
    for round in 0..rounds {
        for (i, f) in files.iter().enumerate() {
            let path_s = f.to_string_lossy().to_string();
            // tpu_append_file — adds one line per round.
            s.assert_tool_success(
                "tpu_append_file",
                json!({"file": path_s, "content": format!("r{round}-line-{i}\n")}),
            );
            // tpu_read_file — exercises the worker on the read path too.
            let read = s.tool_call("tpu_read_file", json!({"file": path_s}));
            assert!(
                read.get("result")
                    .and_then(|r| r.get("isError"))
                    .and_then(|v| v.as_bool())
                    != Some(true),
                "read mid-chaos returned isError: {read}"
            );
            // tpu_replace_in_file — rewrite the seed marker each round.
            // The pattern intentionally consumes any prior `-rN` suffix
            // so each round overwrites the rolling marker cleanly rather
            // than appending to it (replace's match is regex, not
            // anchored, so a bare `seed-{i}` would match inside the
            // previous round's output and accrete suffixes).
            s.assert_tool_success(
                "tpu_replace_in_file",
                json!({
                    "file": path_s,
                    "pattern": format!(r"seed-{i}(?:-r\d+)*"),
                    "replacement": format!("seed-{i}-r{round}"),
                }),
            );
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = killer.join();

    // Each file should end with the round-(rounds-1) replace marker
    // applied to the seed line, plus `rounds` appended lines (one per
    // round-index).  Verify both invariants per file directly off disk.
    for (i, f) in files.iter().enumerate() {
        let body = std::fs::read_to_string(f).expect("read back");
        let expected_seed = format!("seed-{i}-r{}\n", rounds - 1);
        assert!(
            body.starts_with(&expected_seed),
            "file {i} does not start with the final replace marker {expected_seed:?}; got: {body:?}",
        );
        for round in 0..rounds {
            let needle = format!("r{round}-line-{i}\n");
            assert!(
                body.contains(&needle),
                "file {i} missing appended line {needle:?}; got: {body:?}",
            );
        }
    }
}

/// Stage a file that has been stranded mid-atomic-swap (file gone, `.bak`
/// holds the prior contents) and verify that the next MCP read recovers
/// it transparently — no error, content matches what was in the `.bak`.
#[test]
fn stranded_backup_is_auto_recovered_on_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut s = ChaosSession::start();
    s.warmup(tmp.path());

    let target = tmp.path().join("stranded.txt");
    let bak = tmp.path().join("stranded.txt.bak");
    std::fs::write(&bak, b"original content\n").expect("seed .bak");
    assert!(!target.exists(), "precondition: target must be missing");

    let resp = s.assert_tool_success(
        "tpu_read_file",
        json!({"file": target.to_string_lossy()}),
    );
    let text = resp
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("no /result/content/0/text in {resp}"));
    assert!(
        text.contains("original content"),
        "recovered read should yield original content; got: {text:?}",
    );
    assert!(target.exists(), "post: target file should have been recreated");
    assert!(!bak.exists(), "post: .bak should be consumed by the recovery");
}

/// Same as above but the first touch is a mutating call (`tpu_append_file`).
/// Pre-fix, this would return "file does not exist".  Post-fix, the
/// recovery happens first and the append succeeds against the recovered
/// content.
#[test]
fn stranded_backup_is_auto_recovered_on_append() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut s = ChaosSession::start();
    s.warmup(tmp.path());

    let target = tmp.path().join("stranded2.txt");
    let bak = tmp.path().join("stranded2.txt.bak");
    std::fs::write(&bak, b"line one\n").expect("seed .bak");
    assert!(!target.exists());

    s.assert_tool_success(
        "tpu_append_file",
        json!({"file": target.to_string_lossy(), "content": "line two\n"}),
    );

    let body = std::fs::read_to_string(&target).expect("recovered file readable");
    assert_eq!(body, "line one\nline two\n");
    assert!(!bak.exists(), ".bak consumed by recovery before append");
}
