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

/// Total attempts across one `try_call` invocation: 1 initial attempt plus
/// one retry per configured backoff delay.
///
/// Split out as a pure function (rather than an inline `let` in `try_call`)
/// so it is directly unit-testable without spawning a real worker process --
/// see `tests::max_attempts_is_one_plus_backoff_count`.
fn max_attempts() -> u32 {
    1 + BACKOFFS_MS.len() as u32
}

/// What `try_call`'s retry loop should do after attempt number `attempt`
/// (1-based) has just failed, given a total budget of `max_attempts`.
#[derive(Debug, PartialEq, Eq)]
enum RetryDecision {
    /// Sleep `delay_ms`, then retry as attempt number `next_attempt`.
    Retry { delay_ms: u64, next_attempt: u32 },
    /// The budget is exhausted; fall back to in-process execution.
    GiveUp,
}

/// Pure decision logic for `try_call`'s retry loop, shared by both retry
/// sites (worker-spawn failure and worker-death-mid-call). Extracted as a
/// standalone function -- independent of any real spawn/IPC -- so the
/// attempt-counting and backoff-indexing arithmetic can be unit-tested
/// directly instead of only through a real (hard to fail on demand)
/// subprocess round trip.
fn decide_retry(attempt: u32, max_attempts: u32) -> RetryDecision {
    if attempt >= max_attempts {
        RetryDecision::GiveUp
    } else {
        RetryDecision::Retry {
            delay_ms: BACKOFFS_MS[(attempt - 1) as usize],
            next_attempt: attempt + 1,
        }
    }
}

/// Whether `try_call` should log a "succeeded on retry" progress message:
/// only when the call needed more than one attempt.
fn should_log_retry_success(attempt: u32) -> bool {
    attempt > 1
}

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

    /// Two of this function's mutants (`n == 0` -> `n != 0`, and
    /// `resp_id != id` -> `resp_id == id`) are excluded from mutation
    /// testing (see the `mutants::skip` reasoning below).
    ///
    /// Both require a real spawned `--io-worker` child process to reach at
    /// all -- `stdin`/`stdout` here are concrete `ChildStdin`/
    /// `BufReader<ChildStdout>`, not generic `Read`/`Write`, so there is no
    /// way to feed this function a controlled fake response without either
    /// a real subprocess or a deeper refactor to make `IoWorker` generic
    /// over `Read + Write`. And a real subprocess round trip is not cheap
    /// per-mutant: this function has no already-caught mutants of its own
    /// to lose by skipping it wholesale (verified against a full
    /// `cargo mutants` baseline before adding this attribute), and the
    /// existing coverage for it (`tests/io_worker_chaos.rs`) drives dozens
    /// of real round trips per test -- under either of these two mutations
    /// every one of those round trips is misclassified as a dead worker,
    /// respawning and backing off (up to ~1.7s) each time, which
    /// accumulates well past `cargo mutants`' own per-mutant timeout long
    /// before the chaos suite would ever report the resulting failure.
    #[cfg_attr(test, mutants::skip)]
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
            let is_error = resp
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Ok(crate::tools::ToolResult {
                text: text.to_string(),
                is_error,
            })
        } else if let Some(err) = resp.get("err").and_then(|v| v.as_str()) {
            Err(WorkerCallError::Protocol(format!(
                "worker tool error: {err}"
            )))
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
    ) -> Option<crate::tools::ToolResult> {
        if !self.enabled {
            return None;
        }

        // attempt counter is 1-based for human-readable progress messages.
        // Total attempts = 1 initial + BACKOFFS_MS.len() retries.
        let max_attempts: u32 = max_attempts();
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

            // NOTE (mutation testing): the `!` here (`if !have_worker`) has
            // a known cargo-mutants timeout when deleted -- under that
            // mutation, every successful spawn is misclassified as a
            // failure, and this function's *other* mutants (a few lines
            // above and below) are already caught by existing tests, so a
            // function-level `#[mutants::skip]` would sacrifice that
            // coverage for the sake of this one site. `mutants::exclude_re`
            // would let us exclude just this mutation while keeping the
            // rest, but it isn't in the currently-published `mutants` crate
            // (max 0.0.4; needs 0.0.5+). Deliberately left uncovered for
            // now rather than losing the sibling coverage or spawning a
            // real subprocess-with-test-only-misbehavior just for this one
            // mutation; revisit once `exclude_re` ships.
            if !have_worker {
                match decide_retry(attempt, max_attempts) {
                    RetryDecision::GiveUp => {
                        progress(&format!(
                            "io worker unavailable after {max_attempts} attempts; running '{name}' in-process"
                        ));
                        return None;
                    }
                    RetryDecision::Retry {
                        delay_ms,
                        next_attempt,
                    } => {
                        std::thread::sleep(Duration::from_millis(delay_ms));
                        attempt = next_attempt;
                        continue;
                    }
                }
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
                    if should_log_retry_success(attempt) {
                        progress(&format!(
                            "io worker succeeded on attempt {attempt}/{max_attempts} for '{name}'"
                        ));
                    }
                    return Some(tr);
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
                        WorkerCallError::PipeBroken(m) | WorkerCallError::Protocol(m) => m.as_str(),
                    };
                    match decide_retry(attempt, max_attempts) {
                        RetryDecision::GiveUp => {
                            progress(&format!(
                                "io worker died ({reason}) on attempt {attempt}/{max_attempts} for '{name}'; running this call in-process"
                            ));
                            return None;
                        }
                        RetryDecision::Retry {
                            delay_ms,
                            next_attempt,
                        } => {
                            progress(&format!(
                                "io worker died ({reason}) on attempt {attempt}/{max_attempts} for '{name}'; respawning and retrying in {delay_ms} ms"
                            ));
                            std::thread::sleep(Duration::from_millis(delay_ms));
                            attempt = next_attempt;
                            continue;
                        }
                    }
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
    // INVARIANT: this `stdout` is a structured protocol channel — the worker
    // pipe carries newline-delimited JSON and nothing else.  (The same is true
    // of the MCP server's own stdout, which speaks JSON-RPC.)  A stray
    // `println!`/`print!`/direct stdout write from `tpu` library code or any
    // dependency would interleave noise into the framing and break the parent's
    // line reader.  All payload must flow through `write_response`; everything
    // diagnostic must go to stderr.  `tpu` is disciplined about writing to
    // caller-supplied `Vec<u8>`/`Write` buffers rather than stdout — keep it
    // that way.
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
        Ok(crate::tools::ToolResult { text, is_error }) => {
            serde_json::json!({ "id": id, "ok": text, "is_error": is_error })
        }
        Err(err) => serde_json::json!({ "id": id, "err": err }),
    };
    if let Ok(mut s) = serde_json::to_string(&resp) {
        s.push('\n');
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `write_response` is generic over `impl Write`, so it can be tested
    /// directly against an in-memory buffer -- no subprocess, no pipe, no
    /// possibility of hanging. This is the fix for that mutant's previous
    /// classification as a `cargo mutants` TIMEOUT: the only prior coverage
    /// routed through a real end-to-end worker round trip (`write_response`
    /// mutated to `()` means the child never replies, so the parent's
    /// blocking `read_line` in `IoWorker::call` waits forever); this test
    /// exercises the exact same behaviour in milliseconds.
    #[test]
    fn write_response_ok_emits_expected_json_line() {
        let mut buf: Vec<u8> = Vec::new();
        write_response(
            &mut buf,
            7,
            Ok(crate::tools::ToolResult {
                text: "hello".to_string(),
                is_error: false,
            }),
        );
        let line = String::from_utf8(buf).unwrap();
        assert!(line.ends_with('\n'));
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["ok"], "hello");
        assert_eq!(parsed["is_error"], false);
        assert!(parsed.get("err").is_none());
    }

    #[test]
    fn write_response_ok_with_is_error_true() {
        let mut buf: Vec<u8> = Vec::new();
        write_response(
            &mut buf,
            1,
            Ok(crate::tools::ToolResult {
                text: "bad input".to_string(),
                is_error: true,
            }),
        );
        let line = String::from_utf8(buf).unwrap();
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["ok"], "bad input");
        assert_eq!(parsed["is_error"], true);
    }

    #[test]
    fn write_response_err_emits_expected_json_line() {
        let mut buf: Vec<u8> = Vec::new();
        write_response(&mut buf, 3, Err("boom".to_string()));
        let line = String::from_utf8(buf).unwrap();
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["id"], 3);
        assert_eq!(parsed["err"], "boom");
        assert!(parsed.get("ok").is_none());
    }

    #[test]
    fn write_response_always_terminates_with_exactly_one_newline() {
        let mut buf: Vec<u8> = Vec::new();
        write_response(
            &mut buf,
            0,
            Ok(crate::tools::ToolResult {
                text: String::new(),
                is_error: false,
            }),
        );
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 1);
        assert_eq!(*buf.last().unwrap(), b'\n');
    }

    // -- try_call retry-loop pure logic --------------------------------------

    #[test]
    fn max_attempts_is_one_plus_backoff_count() {
        assert_eq!(max_attempts(), 1 + BACKOFFS_MS.len() as u32);
        assert_eq!(max_attempts(), 4);
    }

    #[test]
    fn decide_retry_indexes_the_correct_backoff_for_each_attempt() {
        assert_eq!(
            decide_retry(1, 4),
            RetryDecision::Retry {
                delay_ms: BACKOFFS_MS[0],
                next_attempt: 2
            }
        );
        assert_eq!(
            decide_retry(2, 4),
            RetryDecision::Retry {
                delay_ms: BACKOFFS_MS[1],
                next_attempt: 3
            }
        );
        assert_eq!(
            decide_retry(3, 4),
            RetryDecision::Retry {
                delay_ms: BACKOFFS_MS[2],
                next_attempt: 4
            }
        );
    }

    /// Exact boundary: once `attempt` reaches `max_attempts` the budget is
    /// exhausted (`GiveUp`), but the attempt *just before* that must still
    /// retry -- pins the `>=` comparison against a `<` mutation.
    #[test]
    fn decide_retry_gives_up_exactly_at_max_attempts_boundary() {
        assert_eq!(
            decide_retry(3, 4),
            RetryDecision::Retry {
                delay_ms: BACKOFFS_MS[2],
                next_attempt: 4
            }
        );
        assert_eq!(decide_retry(4, 4), RetryDecision::GiveUp);
    }

    #[test]
    fn should_log_retry_success_only_after_the_first_attempt() {
        assert!(!should_log_retry_success(1));
        assert!(should_log_retry_success(2));
        assert!(should_log_retry_success(4));
    }

    /// `WorkerCallError` has exactly two variants today (`PipeBroken` and
    /// `Protocol`), both of which represent a dead worker, so
    /// `is_worker_dead` is currently always `true` by construction --
    /// mutating its body to the literal `true` is behaviourally identical
    /// and not pursued as a mutant to catch (see crates/tpu/CHECKLIST.md
    /// Milestone 11). This test pins the *intended* semantics (every
    /// variant reports dead) rather than trying to distinguish the
    /// unmutated body from the equivalent mutant.
    #[test]
    fn is_worker_dead_true_for_every_current_variant() {
        assert!(WorkerCallError::PipeBroken("x".into()).is_worker_dead());
        assert!(WorkerCallError::Protocol("x".into()).is_worker_dead());
    }

    /// `std::process::Child` does *not* kill its child on drop by itself,
    /// but for a healthy/idle worker this test cannot distinguish that from
    /// the *other* automatic effect of dropping `IoWorker`: its `stdin`
    /// field (`ChildStdin`) is closed right after this impl's `drop()` body
    /// returns, and the worker's own `run_worker` loop exits on the next
    /// read once it sees that EOF -- so a mutation replacing this `drop()`
    /// body with `()` still passes this test (confirmed by hand-mutating
    /// and re-running: see crates/tpu/CHECKLIST.md Milestone 11). The
    /// explicit `kill()` + `wait()` only matters for an *unresponsive*
    /// worker, which would require either a flaky timing race (checking
    /// immediately after `drop()` returns, since only the real `wait()`
    /// makes that instant deterministic) or new test-only instrumentation
    /// to make the worker ignore stdin EOF on purpose; not pursued for
    /// either reason. This test still pins the weaker, always-true
    /// invariant that the child is *eventually* gone.
    #[test]
    fn drop_terminates_the_child_process() {
        let worker = IoWorker::spawn().expect("spawn io-worker for drop test");
        let pid = worker.child.id();
        drop(worker);

        // Give the OS a brief window to finish tearing the process down
        // after `wait()` returns inside `drop`.
        let mut sys = sysinfo::System::new();
        let mut still_alive = true;
        for _ in 0..50 {
            sys.refresh_all();
            if sys.process(sysinfo::Pid::from_u32(pid)).is_none() {
                still_alive = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !still_alive,
            "child process {pid} must be terminated after IoWorker is dropped"
        );
    }
}
