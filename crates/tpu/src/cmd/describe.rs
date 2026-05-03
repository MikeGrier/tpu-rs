// Copyright (c) 2026, Michael Grier

#![allow(dead_code)] // command implemented but not yet wired into the CLI

//! `tpu describe` — report file metadata: byte count, line count, encoding,
//! line-ending convention, and BOM presence.
//!
//! See [`run`] for the full contract and output guarantees.

use std::{fs, path::Path, sync::Arc};

use harrier::{
    encoding::{LineEnding, SourceConfig},
    lines::LineTerminator,
    source::Source,
};

use crate::IoMode;

// ── DescribeResult ────────────────────────────────────────────────────────────

/// All metadata fields produced by `tpu describe` for one file.
///
/// Fields are documented in DESIGN-NOTES.md (tpu DF section).
pub struct DescribeResult {
    /// Absolute path as supplied by the caller.
    pub file: String,
    /// Raw file size in bytes, from `fs::metadata`.
    pub byte_count: u64,
    /// Number of lines as counted by the harrier `Lines` iterator.
    /// 0 for empty files or files with no content beyond a BOM.
    pub line_count: u64,
    /// WHATWG encoding label, e.g. `"UTF-8"`, `"UTF-16LE"`, `"Windows-1252"`.
    pub encoding: &'static str,
    /// Dominant line-ending convention, or mixed/none as described below.
    /// `"LF"`, `"CRLF"`, `"CR"`, `"Mixed"`, or `"None"`.
    pub line_ending: &'static str,
    /// Whether the file began with a byte-order mark.
    pub bom: bool,
}

// ── LineEndingSeen ─────────────────────────────────────────────────────────────

/// Tracks distinct terminator kinds seen during iteration to detect mixed files.
#[derive(Default)]
struct LineEndingSeen {
    lf: bool,
    crlf: bool,
    cr: bool,
}

impl LineEndingSeen {
    fn record(&mut self, le: LineEnding) {
        match le {
            LineEnding::Lf => self.lf = true,
            LineEnding::CrLf => self.crlf = true,
            LineEnding::Cr => self.cr = true,
        }
    }

    /// Derive the `line_ending` string from the set of terminators seen.
    ///
    /// - `"None"` when no terminated lines were encountered (empty file or
    ///   single unterminated line).
    /// - `"Mixed"` when more than one distinct kind was seen.
    /// - `"LF"` / `"CRLF"` / `"CR"` when exactly one kind was seen.
    fn as_str(&self) -> &'static str {
        match (self.lf, self.crlf, self.cr) {
            (false, false, false) => "None",
            (true, false, false) => "LF",
            (false, true, false) => "CRLF",
            (false, false, true) => "CR",
            _ => "Mixed",
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Collect all metadata for `file` and return a [`DescribeResult`].
///
/// # Output contract
///
/// - `byte_count` is the raw file size from `fs::metadata`, regardless of
///   encoding or BOM.
/// - `line_count` counts the same way harrier does: a trailing newline does
///   **not** add an extra empty line.  Empty file → 0.
/// - `encoding` is the WHATWG label for the detected (or BOM-derived)
///   encoding.
/// - `line_ending` is `"Mixed"` when more than one distinct terminator kind
///   is present; `"None"` when the file is empty or has no terminated lines.
/// - `bom` is true when `Source` detected and skipped a BOM.
///
/// # Errors
///
/// Returns an error on any I/O failure (file not found, permission denied,
/// directory instead of file, etc.).
pub fn run(file: &Path, io_mode: IoMode) -> Result<DescribeResult, Box<dyn std::error::Error>> {
    let file_str = file.to_string_lossy().into_owned();

    // ── byte_count ──────────────────────────────────────────────────────────
    let metadata = fs::metadata(file).map_err(|e| format!("describe: {}: {e}", file.display()))?;
    let byte_count = metadata.len();

    // ── open as harrier Source ───────────────────────────────────────────────
    let branch = crate::open_as_branch(file, io_mode)
        .map_err(|e| format!("describe: open {}: {e}", file.display()))?;
    let source = Source::new(Arc::clone(&branch), SourceConfig::default())
        .map_err(|e| format!("describe: source {}: {e}", file.display()))?;

    let bom = source.bom_len() > 0;
    let encoding_label = source.encoding().name();

    let mut lines = source
        .as_lines()
        .map_err(|e| format!("describe: lines {}: {e}", file.display()))?;

    // ── iterate to count lines and track terminators ────────────────────────
    let mut line_count: u64 = 0;
    let mut seen = LineEndingSeen::default();

    loop {
        match lines.next() {
            None => break,
            Some((_bytes, terminator)) => {
                line_count += 1;
                match terminator {
                    LineTerminator::Ending(le) => seen.record(le),
                    LineTerminator::End => {
                        // Final unterminated line — counts toward line_count
                        // but contributes no terminator kind.
                    }
                }
            }
        }
    }

    let line_ending = seen.as_str();

    Ok(DescribeResult {
        file: file_str,
        byte_count,
        line_count,
        encoding: encoding_label,
        line_ending,
        bom,
    })
}
