// Copyright (c) 2026, Michael Grier

//! Out-of-process I/O worker for `tpu-mcp`.
//!
//! ## Why
//!
//! Windows Defender's minifilter has historically terminated `tpu-mcp`
//! mid-operation when the process performs file I/O at high rates (Defender
//! treats LLVM-built binaries doing rapid file work as suspicious).  Killing
//! the MCP server interrupts the active chat session and forces the user to
//! restart it.
//!
//! ## How
//!
//! When enabled (default on Windows), `tpu-mcp` spawns one child of itself
//! via `--io-worker` and forwards every tool call to it over an anonymous
//! stdin/stdout pipe pair.  The child runs the *exact same* `tools::call`
//! dispatch path the parent would use in-process, so behaviour is identical;
//! only the address space hosting the I/O changes.  If Defender (or any
//! other failure) kills the worker, the parent survives: the manager
//! observes a broken pipe / EOF, emits a warning to the MCP client via
//! `notifications/message`, and respawns a fresh worker before retrying the
//! failing call once. If the retry also crashes — or the worker cannot be
//! spawned in the first place — the manager transparently falls back to
//! performing the call in-process so the user-visible operation still succeeds.
//!
//! ## What about an atomic-write swap that crashes mid-rename?
//!
//! The existing write path (`tempfile::NamedTempFile` → rename original to
//! `<file>.bak` → persist temp → original path) has a small window where a
//! crash between the two renames leaves the original at `<file>.bak` and
//! no file at the original path.  This was already true in-process; out-of-
//! process isolation does not make it worse.  On retry the operation
//! reruns from scratch, which works as long as the input file is still
//! present (the operating assumption noted in the design discussion).
//! Recovery for the stranded-`.bak` case is provided by
//! [`tpu::recover_stranded_backup`], which is invoked automatically by
//! the read helpers and by the mutating `cmd::*` entry points so the
//! original path is restored before the next operation runs.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::Mutex,
    time::Duration,
};

use serde_json::Value;

use crate::tools::ServerConfig;

/// CLI argument that switches `tpu-mcp` from MCP-server mode into
/// I/O-worker mode.  Recognised when present anywhere on the command line.
pub const WORKER_ARG: &str = "--io-worker";

/// CLI argument that disables out-of-process I/O isolation, forcing every
/// tool call to run in the MCP-server process directly.
pub const DISABLE_ARG: &str = "--no-io-worker";

/// Backoff schedule between successive respawn attempts.  Escalates because
/// Defender rate-limiting is one of the suspected causes of worker death,
/// and burning extra wall-clock between retries is cheap relative to the
/// alternative (the user's chat session breaks).  After `BACKOFFS_MS.len()`
/// failed retries the call falls back to in-process execution so the
/// user-visible operation still succeeds.
const BACKOFFS_MS: &[u64] = &[200, 500, 1000];

// -- per-process worker handle ------------------------------------------------

/// Connection to a single child `tpu-mcp --io-worker` process.
struct IoWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl IoWorker {
    fn spawn() -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let mut child = Command::new(exe)
            .arg(WORKER_ARG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Worker stderr is shared with the parent so any panic backtrace
            // surfaces in the MCP output channel for diagnosis.
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    fn call(
        &mut self,
        name: &str,
        args: &Value,
        config: &ServerConfig,
    ) -> Result<crate::tools::ToolResult, WorkerCallError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        let req = serde_json::json!({
            "id": id,
            "name": name,
            "args": args,
            "config": config.to_wire(),
        });
        let mut line = serde_json::to_string(&req)
            .map_err(|e| WorkerCallError::Protocol(format!("encode request: {e}")))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| WorkerCallError::PipeBroken(format!("write: {e}")))?;
        self.stdin
            .flush()
            .map_err(|e| WorkerCallError::PipeBroken(format!("flush: {e}")))?;

        let mut resp_line = String::new();
        let n = self
            .stdout
            .read_line(&mut resp_line)
            .map_err(|e| WorkerCallError::PipeBroken(format!("read: {e}")))?;
        if n == 0 {
            return Err(WorkerCallError::PipeBroken("worker closed stdout".into()));
        }
        let resp: Value = serde_json::from_str(resp_line.trim()).map_err(|e| {
            WorkerCallError::Protocol(format!("decode response: {e}; raw: {resp_line:?}"))
        })?;
        let resp_id = resp.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        if resp_id != id {
            return Err(WorkerCallError::Protocol(format!(
                "id mismatch: sent {id} got {resp_id}"
            )));
        }
        if let Some(text) = resp.get("ok").and_then(|v| v.as_str()) {
            let is_error = resp.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
            Ok(crate::tools::ToolResult { text: text.to_string(), is_error })
        } else if let Some(err) = resp.get("err").and_then(|v| v.as_str()) {
            Err(WorkerCallError::Protocol(format!("worker tool error: {err}")))
        } else {
            Err(WorkerCallError::Protocol(
                "response had neither ok nor err".into(),
            ))
        }
    }
}

impl Drop for IoWorker {
    fn drop(&mut self) {
        // Closing stdin signals EOF; the worker exits on the next read.
        // Force-kill any worker that does not respond promptly so we never
        // leak children if the parent is going down.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Reasons a worker call can fail.  Distinguishes "the worker is gone" from
/// wire-protocol errors.
enum WorkerCallError {
    /// Pipe error (broken pipe, EOF) — worker is unusable and must be
    /// respawned.
    PipeBroken(String),
    /// Wire-protocol error (malformed JSON, id mismatch) — also fatal.
    Protocol(String),
}

impl WorkerCallError {
    fn is_worker_dead(&self) -> bool {
        matches!(self, Self::PipeBroken(_) | Self::Protocol(_))
    }
}

// -- public handle ------------------------------------------------------------

/// Public handle to the I/O worker subsystem.  Cheap to clone-via-`Arc`,
/// safe to call from any thread (calls serialise on an internal mutex).
pub struct IoWorkerHandle {
    inner: Mutex<Option<IoWorker>>,
    enabled: bool,
}

impl IoWorkerHandle {
    /// Build a handle that performs every call in-process.  Use this when
    /// the user passes `--no-io-worker` or on platforms where Defender is
    /// not a concern.
    pub fn disabled() -> Self {
        Self {
            inner: Mutex::new(None),
            enabled: false,
        }
    }

    /// Build a handle that lazily spawns a worker on first use.  Spawning
    /// is deferred so that startup failures degrade gracefully into the
    /// in-process fallback rather than aborting `tpu-mcp` outright.
    pub fn enabled() -> Self {
        Self {
            inner: Mutex::new(None),
            enabled: true,
        }
    }

    /// Run `name(args)` in the worker process.
    ///
    /// `progress` is invoked once for each retryable event — worker spawn
    /// failure, worker death, or exhaustion of the retry budget.  Callers
    /// route it to MCP `notifications/message` so the user sees worker
    /// turbulence in their chat UI instead of having to look at stderr.
    ///
    /// Returns:
    /// - `Ok(Some(tr))` — the worker executed the tool and returned a
    ///   [`ToolResult`].  The result may represent either a successful
    ///   outcome (`tr.is_error == false`) or a tool-level failure
    ///   (`tr.is_error == true`); both are propagated verbatim to the MCP
    ///   client.  Transparent retries may have occurred.
    /// - `Ok(None)` — all attempts were exhausted (spawn failure, pipe
    ///   broken, or protocol error) or the worker subsystem is disabled.
    ///   The caller should run `tools::call` in-process and use that result.
    ///
    /// This function never returns `Err`: every worker failure is handled
    /// internally by retrying up to `max_attempts` times and then
    /// falling back to `Ok(None)`.
    pub fn try_call(
        &self,
        name: &str,
        args: &Value,
        config: &ServerConfig,
        progress: &mut dyn FnMut(&str),
    ) -> Result<Option<crate::tools::ToolResult>, Box<dyn std::error::Error>> {
        if !self.enabled {
            return Ok(None);
        }

        // attempt counter is 1-based for human-readable progress messages.
        // Total attempts = 1 initial + BACKOFFS_MS.len() retries.
        let max_attempts: u32 = 1 + BACKOFFS_MS.len() as u32;
        let mut attempt: u32 = 1;

        loop {
            // Phase A: make sure we have a worker.  Spawn on demand; if
            // spawning fails, treat the same as a worker death — back off
            // and try again, falling through to in-process on exhaustion.
            let have_worker = {
                let mut guard = self.inner.lock().expect("io-worker mutex poisoned");
                if guard.is_some() {
                    true
                } else {
                    match IoWorker::spawn() {
                        Ok(w) => {
                            *guard = Some(w);
                            true
                        }
                        Err(e) => {
                            progress(&format!(
                                "io worker spawn failed on attempt {attempt}/{max_attempts} for '{name}': {e}"
                            ));
                            false
                        }
                    }
                }
            };

            if !have_worker {
                if attempt >= max_attempts {
                    progress(&format!(
                        "io worker unavailable after {max_attempts} attempts; running '{name}' in-process"
                    ));
                    return Ok(None);
                }
                let delay_ms = BACKOFFS_MS[(attempt - 1) as usize];
                std::thread::sleep(Duration::from_millis(delay_ms));
                attempt += 1;
                continue;
            }

            // Phase B: make the call.
            let result = {
                let mut guard = self.inner.lock().expect("io-worker mutex poisoned");
                guard
                    .as_mut()
                    .expect("worker present after spawn")
                    .call(name, args, config)
            };

            match result {
                Ok(tr) => {
                    if attempt > 1 {
                        progress(&format!(
                            "io worker succeeded on attempt {attempt}/{max_attempts} for '{name}'"
                        ));
                    }
                    return Ok(Some(tr));
                }
                Err(e) => {
                    debug_assert!(e.is_worker_dead());
                    // Drop the dead worker; the next loop iteration will
                    // respawn it (or fall through to in-process).
                    {
                        let mut guard = self.inner.lock().expect("io-worker mutex poisoned");
                        *guard = None;
                    }
                    let reason = match &e {
                        WorkerCallError::PipeBroken(m) | WorkerCallError::Protocol(m) => {
                            m.as_str()
                        }
                    };
                    if attempt >= max_attempts {
                        progress(&format!(
                            "io worker died ({reason}) on attempt {attempt}/{max_attempts} for '{name}'; running this call in-process"
                        ));
                        return Ok(None);
                    }
                    let delay_ms = BACKOFFS_MS[(attempt - 1) as usize];
                    progress(&format!(
                        "io worker died ({reason}) on attempt {attempt}/{max_attempts} for '{name}'; respawning and retrying in {delay_ms} ms"
                    ));
                    std::thread::sleep(Duration::from_millis(delay_ms));
                    attempt += 1;
                    continue;
                }
            }
        }
    }
}

// -- worker entry point -------------------------------------------------------

/// Run as the I/O worker child.  Reads newline-delimited JSON requests from
/// stdin, dispatches each through `tools::call`, and writes a newline-
/// delimited JSON response to stdout for every request.  Exits cleanly when
/// stdin closes (the parent dropping its end of the pipe).
///
/// The wire format is intentionally tiny — request and response are both
/// JSON objects with an `id` and one of `name+args+config` or `ok|err`.
/// We do not reuse the MCP JSON-RPC envelope because the worker is a
/// private subprocess of `tpu-mcp`, not an MCP server itself.
pub fn run_worker() -> ! {
    use std::io::{self, BufRead};

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                write_response(&mut out, 0, Err(format!("worker: parse: {e}")));
                continue;
            }
        };
        let id = req.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        let name = req
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let args = req.get("args").cloned().unwrap_or(Value::Null);
        let config_val = req.get("config").cloned().unwrap_or(Value::Null);
        let config = match ServerConfig::from_wire(&config_val) {
            Ok(c) => c,
            Err(e) => {
                write_response(&mut out, id, Err(format!("worker: bad config: {e}")));
                continue;
            }
        };

        let result = crate::tools::call(&name, &args, &config).map_err(|e| format!("{e}"));
        write_response(&mut out, id, result);
    }

    std::process::exit(0);
}

fn write_response(out: &mut impl Write, id: u64, result: Result<crate::tools::ToolResult, String>) {
    let resp = match result {
        Ok(crate::tools::ToolResult { text, is_error }) => serde_json::json!({ "id": id, "ok": text, "is_error": is_error }),
        Err(err) => serde_json::json!({ "id": id, "err": err }),
    };
    if let Ok(mut s) = serde_json::to_string(&resp) {
        s.push('\n');
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }
}
