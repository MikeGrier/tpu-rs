// Copyright (c) 2026, Michael Grier

//! Injectable, colour-aware diagnostics writer.
//!
//! All human-readable status, warning, and error messages go through [`Shell`].
//! Payload (file content, diffs) is delivered via [`crate::output::Output`];
//! Shell is **diagnostics-only** — it never touches payload bytes.
//!
//! In human mode (default) diagnostics go to stderr.  In JSON mode
//! (`--message-format=json`) diagnostics are emitted as NDJSON objects on
//! stdout, mirroring Cargo's `--message-format=json` convention.
//!
//! See [`crate::message::Msg`] for the NDJSON schema.
//!
//! [`Shell::new`] — human mode, real stderr with TTY colour detection.  
//! [`Shell::new_json`] — JSON mode, real stdout.  
//! [`Shell::from_write`] / [`Shell::from_json_write`] — test sinks.

use std::{
    fmt,
    io::{self, Write},
};

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Color, Style};

use crate::message::{MessageFormat, Msg};

// ── Styles ────────────────────────────────────────────────────────────────────
// Terminal-only; carry no semantic meaning, not part of any protocol.

const ERROR_STYLE: Style = Style::new()
    .bold()
    .fg_color(Some(Color::Ansi(AnsiColor::Red)));

#[allow(dead_code)]
const WARN_STYLE: Style = Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow)));

const STATUS_STYLE: Style = Style::new()
    .bold()
    .fg_color(Some(Color::Ansi(AnsiColor::Green)));

// ── Verbosity ─────────────────────────────────────────────────────────────────

/// How much output `Shell` emits in human mode.
///
/// In JSON mode verbosity is ignored for `status`/`warning` — machine
/// consumers always receive every message.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    /// Suppress all status and warning messages; errors still appear.
    Quiet,
    /// Normal operation (default).
    Normal,
    /// Emit additional diagnostic detail.
    Verbose,
}

// ── Shell ─────────────────────────────────────────────────────────────────────

/// Injectable diagnostics writer with colour support and verbosity control.
///
/// All subcommands receive `&mut Shell` explicitly; no `eprintln!` or direct
/// `println!` calls appear in library or dispatch code — all diagnostics go
/// through Shell so the mode is respected uniformly.  Payload bytes (file
/// content, diffs) are delivered via [`crate::output::Output`], not Shell.
pub struct Shell {
    /// Destination for all Shell output.
    ///
    /// Human mode: coloured stderr.  JSON mode: NDJSON stdout.
    out: Box<dyn Write + Send>,
    format: MessageFormat,
    verbosity: Verbosity,
}

#[allow(dead_code)]
impl Shell {
    /// Creates a human-mode `Shell` writing coloured messages to real stderr.
    pub fn new() -> Self {
        Shell {
            out: Box::new(AutoStream::new(io::stderr(), ColorChoice::Auto)),
            format: MessageFormat::Human,
            verbosity: Verbosity::Normal,
        }
    }

    /// Creates a JSON-mode `Shell` writing NDJSON diagnostics to real stdout.
    pub fn new_json() -> Self {
        Shell {
            out: Box::new(io::stdout()),
            format: MessageFormat::Json,
            verbosity: Verbosity::Normal,
        }
    }

    /// Creates a human-mode `Shell` that writes to `w` with ANSI stripped.
    ///
    /// Intended for unit tests: pass a `Vec<u8>` or similar sink.
    pub fn from_write(w: Box<dyn Write + Send>) -> Self {
        Shell {
            out: Box::new(AutoStream::never(w)),
            format: MessageFormat::Human,
            verbosity: Verbosity::Normal,
        }
    }

    /// Creates a JSON-mode `Shell` that writes NDJSON diagnostics to `w`.
    ///
    /// Intended for unit tests: pass a `Vec<u8>` or similar sink.
    pub fn from_json_write(w: Box<dyn Write + Send>) -> Self {
        Shell {
            out: w,
            format: MessageFormat::Json,
            verbosity: Verbosity::Normal,
        }
    }

    /// Returns `true` when JSON mode is active.
    pub fn is_json(&self) -> bool {
        matches!(self.format, MessageFormat::Json)
    }

    /// Updates the verbosity level (human mode only; ignored in JSON mode).
    pub fn set_verbosity(&mut self, v: Verbosity) {
        self.verbosity = v;
    }

    /// Returns the current verbosity level.
    pub fn verbosity(&self) -> Verbosity {
        self.verbosity
    }

    // ── Human-mode message writers ────────────────────────────────────────────

    /// Prints a right-aligned, bold green status label followed by a message.
    ///
    /// - Human mode: suppressed in [`Verbosity::Quiet`].
    /// - JSON mode: always emitted as `{"reason":"status",...}`.
    pub fn status<S, M>(&mut self, verb: S, message: M) -> io::Result<()>
    where
        S: fmt::Display,
        M: fmt::Display,
    {
        if self.format == MessageFormat::Json {
            let v = verb.to_string();
            let m = message.to_string();
            return self.emit_json(&Msg::Status {
                verb: &v,
                message: &m,
            });
        }
        if self.verbosity == Verbosity::Quiet {
            return Ok(());
        }
        writeln!(
            self.out,
            "{STATUS_STYLE}{verb:>12}{STATUS_STYLE:#} {message}"
        )
    }

    /// Prints a yellow `warning: …` line.
    ///
    /// - Human mode: suppressed in [`Verbosity::Quiet`].
    /// - JSON mode: always emitted as `{"reason":"warning",...}`.
    pub fn warn<M: fmt::Display>(&mut self, message: M) -> io::Result<()> {
        if self.format == MessageFormat::Json {
            let m = message.to_string();
            return self.emit_json(&Msg::Warning { message: &m });
        }
        if self.verbosity == Verbosity::Quiet {
            return Ok(());
        }
        writeln!(self.out, "{WARN_STYLE}warning{WARN_STYLE:#}: {message}")
    }

    /// Prints a bold red `error: …` line.
    ///
    /// Never suppressed in either mode.
    pub fn error<M: fmt::Display>(&mut self, message: M) -> io::Result<()> {
        if self.format == MessageFormat::Json {
            let m = message.to_string();
            return self.emit_json(&Msg::Error { message: &m });
        }
        writeln!(self.out, "{ERROR_STYLE}error{ERROR_STYLE:#}: {message}")
    }

    /// Direct access to the underlying diagnostics writer for raw writes.
    pub fn err(&mut self) -> &mut dyn Write {
        &mut *self.out
    }

    // ── JSON diagnostic emitters ──────────────────────────────────────────────

    /// Emits a `finished` JSON line.  Only meaningful / called in JSON mode.
    ///
    /// Always the last line emitted for an invocation.
    pub fn emit_finished(&mut self, success: bool) -> io::Result<()> {
        self.emit_json(&Msg::Finished { success })
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    fn emit_json(&mut self, msg: &Msg<'_>) -> io::Result<()> {
        let line =
            serde_json::to_string(msg).map_err(io::Error::other)?;
        writeln!(self.out, "{line}")
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    // ── Test helpers ──────────────────────────────────────────────────────────

    struct VecWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    unsafe impl Send for VecWriter {}

    fn capture_human() -> (Shell, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>> = Default::default();
        let writer = Box::new(VecWriter(buf.clone())) as Box<dyn Write + Send>;
        (Shell::from_write(writer), buf)
    }

    fn capture_json() -> (Shell, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>> = Default::default();
        let writer = Box::new(VecWriter(buf.clone())) as Box<dyn Write + Send>;
        (Shell::from_json_write(writer), buf)
    }

    fn read_lines(buf: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<Value> {
        let bytes = buf.lock().unwrap().clone();
        let s = String::from_utf8(bytes).expect("UTF-8");
        s.lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).expect("valid JSON"))
            .collect()
    }

    // ── Human mode: status / warn / error ────────────────────────────────────

    #[test]
    fn status_writes_to_stderr_sink() {
        let (mut shell, buf) = capture_human();
        shell.status("Compile", "foo.rs").unwrap();
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(out.contains("Compile"), "expected 'Compile' in: {out:?}");
        assert!(out.contains("foo.rs"), "expected 'foo.rs' in: {out:?}");
    }

    #[test]
    fn warn_writes_warning_prefix() {
        let (mut shell, buf) = capture_human();
        shell.warn("something fishy").unwrap();
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(out.contains("warning"), "expected 'warning' in: {out:?}");
        assert!(out.contains("something fishy"), "{out:?}");
    }

    #[test]
    fn error_writes_error_prefix() {
        let (mut shell, buf) = capture_human();
        shell.error("boom").unwrap();
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(out.contains("error"), "expected 'error' in: {out:?}");
        assert!(out.contains("boom"), "{out:?}");
    }

    #[test]
    fn quiet_suppresses_status_and_warn() {
        let (mut shell, buf) = capture_human();
        shell.set_verbosity(Verbosity::Quiet);
        shell.status("Noop", "ignored").unwrap();
        shell.warn("also ignored").unwrap();
        assert!(
            buf.lock().unwrap().is_empty(),
            "expected no output in Quiet mode"
        );
    }

    #[test]
    fn quiet_does_not_suppress_error() {
        let (mut shell, buf) = capture_human();
        shell.set_verbosity(Verbosity::Quiet);
        shell.error("always visible").unwrap();
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(out.contains("always visible"), "{out:?}");
    }

    #[test]
    fn status_right_aligns_to_12_chars() {
        let (mut shell, buf) = capture_human();
        shell.status("hi", "msg").unwrap();
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(out.contains("          hi"), "wrong alignment: {out:?}");
    }

    #[test]
    fn verbosity_default_is_normal() {
        let shell = Shell::new();
        assert_eq!(shell.verbosity(), Verbosity::Normal);
    }

    #[test]
    fn set_verbosity_roundtrip() {
        let mut shell = Shell::new();
        shell.set_verbosity(Verbosity::Verbose);
        assert_eq!(shell.verbosity(), Verbosity::Verbose);
        shell.set_verbosity(Verbosity::Quiet);
        assert_eq!(shell.verbosity(), Verbosity::Quiet);
    }

    #[test]
    fn err_write_raw() {
        let (mut shell, buf) = capture_human();
        shell.err().write_all(b"raw bytes").unwrap();
        let out = buf.lock().unwrap().clone();
        assert_eq!(out, b"raw bytes");
    }

    #[test]
    fn multiple_statuses_append() {
        let (mut shell, buf) = capture_human();
        shell.status("First", "one").unwrap();
        shell.status("Second", "two").unwrap();
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(out.contains("First"), "{out:?}");
        assert!(out.contains("Second"), "{out:?}");
    }

    #[test]
    fn error_and_warn_coexist() {
        let (mut shell, buf) = capture_human();
        shell.warn("careful").unwrap();
        shell.error("fatal").unwrap();
        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(out.contains("warning"), "{out:?}");
        assert!(out.contains("error"), "{out:?}");
    }

    // ── JSON mode: is_json ────────────────────────────────────────────────────

    #[test]
    fn human_mode_is_not_json() {
        let shell = Shell::new();
        assert!(!shell.is_json());
    }

    #[test]
    fn json_mode_is_json() {
        let (shell, _) = capture_json();
        assert!(shell.is_json());
    }

    // ── JSON mode: status / warn / error emit NDJSON ─────────────────────────

    #[test]
    fn json_status_emits_ndjson() {
        let (mut shell, buf) = capture_json();
        shell.status("replace", "file.txt: 3 replacements").unwrap();
        let lines = read_lines(&buf);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["reason"], "status");
        assert_eq!(lines[0]["verb"], "replace");
        assert_eq!(lines[0]["message"], "file.txt: 3 replacements");
    }

    #[test]
    fn json_warn_emits_ndjson() {
        let (mut shell, buf) = capture_json();
        shell.warn("something odd").unwrap();
        let lines = read_lines(&buf);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["reason"], "warning");
        assert_eq!(lines[0]["message"], "something odd");
    }

    #[test]
    fn json_error_emits_ndjson() {
        let (mut shell, buf) = capture_json();
        shell.error("fatal error").unwrap();
        let lines = read_lines(&buf);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["reason"], "error");
        assert_eq!(lines[0]["message"], "fatal error");
    }

    #[test]
    fn json_status_not_suppressed_in_quiet() {
        // JSON mode ignores verbosity: machine consumers always want messages.
        let (mut shell, buf) = capture_json();
        shell.set_verbosity(Verbosity::Quiet);
        shell.status("do", "something").unwrap();
        let lines = read_lines(&buf);
        assert_eq!(
            lines.len(),
            1,
            "JSON status must not be suppressed by Quiet"
        );
    }

    #[test]
    fn json_warn_not_suppressed_in_quiet() {
        let (mut shell, buf) = capture_json();
        shell.set_verbosity(Verbosity::Quiet);
        shell.warn("quiet-but-still-visible").unwrap();
        let lines = read_lines(&buf);
        assert_eq!(
            lines.len(),
            1,
            "JSON warning must not be suppressed by Quiet"
        );
    }

    // ── JSON mode: emit_finished ──────────────────────────────────────────────

    #[test]
    fn emit_finished_true() {
        let (mut shell, buf) = capture_json();
        shell.emit_finished(true).unwrap();
        let lines = read_lines(&buf);
        assert_eq!(lines[0]["reason"], "finished");
        assert_eq!(lines[0]["success"], true);
    }

    #[test]
    fn emit_finished_false() {
        let (mut shell, buf) = capture_json();
        shell.emit_finished(false).unwrap();
        let lines = read_lines(&buf);
        assert_eq!(lines[0]["success"], false);
    }

    // ── JSON mode: multi-message ordering ────────────────────────────────────

    #[test]
    fn json_messages_appear_in_order() {
        let (mut shell, buf) = capture_json();
        shell.status("a", "first").unwrap();
        shell.warn("second").unwrap();
        shell.emit_finished(true).unwrap();
        let lines = read_lines(&buf);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["reason"], "status");
        assert_eq!(lines[1]["reason"], "warning");
        assert_eq!(lines[2]["reason"], "finished");
    }

    #[test]
    fn json_error_then_finished_false() {
        let (mut shell, buf) = capture_json();
        shell.error("it broke").unwrap();
        shell.emit_finished(false).unwrap();
        let lines = read_lines(&buf);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["reason"], "error");
        assert_eq!(lines[1]["success"], false);
    }

    // ── JSON: content with embedded newlines is escaped ───────────────────────

    #[test]
    fn json_message_with_newlines_is_single_ndjson_line() {
        let (mut shell, buf) = capture_json();
        shell.status("read", "line1\nline2\n").unwrap();
        let bytes = buf.lock().unwrap().clone();
        let s = String::from_utf8(bytes).unwrap();
        // Must be exactly one non-empty line.
        let non_empty: Vec<&str> = s.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(non_empty.len(), 1, "expected single NDJSON line: {s:?}");
    }
}
