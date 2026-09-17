// Copyright (c) 2026, Michael Grier

#![allow(dead_code)] // command implemented but not yet wired into the CLI

//! `tpu describe` — report file metadata: byte count, line count, encoding,
//! line-ending convention, and BOM presence.
//!
//! See [`run`] for the full contract and output guarantees.

use std::{fs, path::Path};

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
    /// Terminators of each convention.  `line_ending` collapses these to one
    /// label; the counts are the evidence behind it, and the only way to see
    /// how lopsided a `"Mixed"` file is.
    pub lf_count: u64,
    pub crlf_count: u64,
    pub cr_count: u64,
    /// Whether the file began with a byte-order mark.
    pub bom: bool,
}

/// Map a terminator census to the reported label.
///
/// - `"None"` when no terminated lines were encountered (empty file or
///   single unterminated line).
/// - `"Mixed"` when more than one distinct kind is present.
/// - `"LF"` / `"CRLF"` / `"CR"` when exactly one kind is present.
fn line_ending_label(layout: &crate::TextLayout) -> &'static str {
    match (layout.has_lf(), layout.has_crlf(), layout.has_cr()) {
        (false, false, false) => "None",
        (true, false, false) => "LF",
        (false, true, false) => "CRLF",
        (false, false, true) => "CR",
        _ => "Mixed",
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
/// - `lf_count` / `crlf_count` / `cr_count` are the per-convention terminator
///   counts the label is derived from.
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

    let decoded = crate::read_text_file(file, io_mode)
        .map_err(|e| format!("describe: read {}: {e}", file.display()))?;
    let bom = decoded.bom_len > 0;
    let encoding_label = decoded.encoding.name();
    let line_count = decoded.layout.line_count;
    let line_ending = line_ending_label(&decoded.layout);

    Ok(DescribeResult {
        file: file_str,
        byte_count,
        line_count,
        encoding: encoding_label,
        line_ending,
        lf_count: decoded.layout.lf,
        crlf_count: decoded.layout.crlf,
        cr_count: decoded.layout.cr,
        bom,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_tmp(dir: &TempDir, name: &str, content: &[u8]) -> std::path::PathBuf {
        let p = dir.path().join(name);
        fs::write(&p, content).unwrap();
        p
    }

    // ── line_ending_label ─────────────────────────────────────────────────────
    //
    // Each combination is asserted individually (not just checked against
    // "not None"/"not Mixed") so that a whole-function-replace mutation
    // (returning "" or "xyzzy" unconditionally) and a single match-arm
    // deletion (falling through to the `_ => "Mixed"` catch-all) are both
    // caught: a deleted-arm mutant would still return a *plausible*-looking
    // string ("Mixed") for that one combination, so each expected value must
    // be checked exactly.

    fn layout(lf: u64, crlf: u64, cr: u64) -> crate::TextLayout {
        crate::TextLayout {
            line_count: lf + crlf + cr,
            lf,
            crlf,
            cr,
        }
    }

    #[test]
    fn line_ending_label_none_when_nothing_seen() {
        assert_eq!(line_ending_label(&layout(0, 0, 0)), "None");
    }

    #[test]
    fn line_ending_label_lf_only() {
        assert_eq!(line_ending_label(&layout(3, 0, 0)), "LF");
    }

    #[test]
    fn line_ending_label_crlf_only() {
        assert_eq!(line_ending_label(&layout(0, 3, 0)), "CRLF");
    }

    #[test]
    fn line_ending_label_cr_only() {
        assert_eq!(line_ending_label(&layout(0, 0, 3)), "CR");
    }

    #[test]
    fn line_ending_label_mixed_when_more_than_one_kind() {
        assert_eq!(line_ending_label(&layout(1, 1, 0)), "Mixed");
    }

    /// The counts are the point: a label alone cannot say how lopsided a
    /// mixed file is, which is what makes a single stray CRLF findable.
    #[test]
    fn describe_reports_the_terminator_census() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("mixed.txt");
        fs::write(&f, b"a\nb\r\nc\n").unwrap();

        let r = run(&f, IoMode::Buffered).expect("describe");
        assert_eq!(r.line_ending, "Mixed");
        assert_eq!((r.lf_count, r.crlf_count, r.cr_count), (2, 1, 0));
    }

    // ── run(): bom detection ──────────────────────────────────────────────────

    /// `bom_len` is a `usize`; pins `> 0` against a `<` mutation (always
    /// false for an unsigned type) and against `==`/`>=` by covering both a
    /// BOM'd and a BOM-less file.
    #[test]
    fn run_reports_bom_true_when_file_has_utf8_bom() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"\xEF\xBB\xBFhello\n");
        let result = run(&p, IoMode::Mmap).unwrap();
        assert!(result.bom, "expected bom == true for a BOM'd file");
    }

    #[test]
    fn run_reports_bom_false_when_file_has_no_bom() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"hello\n");
        let result = run(&p, IoMode::Mmap).unwrap();
        assert!(!result.bom, "expected bom == false for a plain file");
    }
}
