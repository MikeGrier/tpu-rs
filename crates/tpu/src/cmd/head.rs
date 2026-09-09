// Copyright (c) 2026, Michael Grier

//! `tpu head` — emit the first N lines or N bytes of a file to stdout.

use std::{io::Write, path::Path};

use harrier::encoding::LineEnding;

use crate::IoMode;

/// Selects how many lines or bytes to emit.
pub enum HeadMode {
    /// Emit the first `n` logical lines.  Default n = 10.
    ///
    /// When `numbers` is true each output line is prefixed with its 1-based
    /// file-line number and a tab; output always uses LF line endings.
    Lines { n: usize, numbers: bool },
    /// Emit the first `n` raw bytes verbatim.  No encoding or line-ending
    /// processing is applied.
    Bytes { n: u64 },
}

/// Run the `head` subcommand.
///
/// Opens `file` and emits the first records selected by `mode` to `out`.
///
/// In `Lines` mode harrier detects the file encoding and line ending; output
/// is always UTF-8, re-emitted using the file's native terminator sequence.
///
/// In `Bytes` mode the raw byte stream is read and the first `n` bytes are
/// written verbatim; no encoding or line-ending processing is applied.
///
/// If the file contains fewer lines / bytes than requested, all are emitted
/// without error.  No trailing newline is added beyond what the file already
/// provides.
///
/// `notes` is the optional advisory writer (Milestone 4).  When `Some` and
/// `mode` is `Lines`, after decoding the file's text
/// [`crate::mojibake::emit_read_advisory`] may emit a `note: <path>: …`
/// line if mojibake is detected.  `Bytes` mode never decodes and so never
/// emits an advisory.  Pass `None` to suppress entirely.
pub fn run(
    file: &Path,
    mode: HeadMode,
    out: &mut dyn Write,
    io_mode: IoMode,
    notes: Option<&mut dyn Write>,
) -> Result<(), Box<dyn std::error::Error>> {
    match mode {
        HeadMode::Lines { n, numbers } => run_lines(file, n, numbers, out, io_mode, notes),
        HeadMode::Bytes { n } => run_bytes(file, n, out, io_mode),
    }
}

/// Emit the first `n` lines of `file` to `out`, using the file's detected
/// line ending as the terminator (or LF when `numbers` is true).  Output is
/// always UTF-8.
fn run_lines(
    file: &Path,
    n: usize,
    numbers: bool,
    out: &mut dyn Write,
    io_mode: IoMode,
    notes: Option<&mut dyn Write>,
) -> Result<(), Box<dyn std::error::Error>> {
    let f = std::fs::File::open(file)?;
    // Empty file → zero lines; mapping a 0-byte file is platform-dependent.
    if f.metadata()?.len() == 0 {
        return Ok(());
    }
    drop(f);

    let decoded = crate::read_text_file(file, io_mode)?;
    let line_ending = decoded.line_ending;
    let text = decoded.text;

    // Read-time advisory (Milestone 4).
    if let Some(notes) = notes {
        crate::mojibake::emit_read_advisory(notes, file, &text)?;
    }

    // Split on the LF that harrier normalises all line endings to.
    let all_parts: Vec<&str> = text.split('\n').collect();
    // A trailing '\n' produces a trailing empty string from split(); record
    // whether the file ended with a newline before dropping the empty element.
    let file_ends_with_newline = all_parts.last() == Some(&"");
    let all_lines: &[&str] = if file_ends_with_newline {
        &all_parts[..all_parts.len() - 1]
    } else {
        &all_parts
    };

    let take = n.min(all_lines.len());
    let terminator: &[u8] = match line_ending {
        LineEnding::Lf => b"\n",
        LineEnding::CrLf => b"\r\n",
        LineEnding::Cr => b"\r",
    };

    for (i, line) in all_lines[..take].iter().enumerate() {
        if numbers {
            // Numbered mode: always LF, no native-terminator logic.
            writeln!(out, "{}\t{}", i + 1, line)?;
        } else {
            out.write_all(line.as_bytes())?;
            // Emit the native terminator unless this is the last selected line
            // and the original file did not terminate it (i.e., the file has no
            // trailing newline and we are emitting its final line).
            let is_last_selected = i + 1 == take;
            let had_terminator =
                !is_last_selected || take < all_lines.len() || file_ends_with_newline;
            if had_terminator {
                out.write_all(terminator)?;
            }
        }
    }

    Ok(())
}

/// Emit the first `n` raw bytes of `file` to `out` with no encoding or
/// line-ending processing.  If the file is shorter than `n` bytes all bytes
/// are emitted without error.
fn run_bytes(
    file: &Path,
    n: u64,
    out: &mut dyn Write,
    io_mode: IoMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let file_len = std::fs::metadata(file)?.len();
    if file_len == 0 || n == 0 {
        return Ok(());
    }
    let bytes = crate::read_raw_bytes(file, io_mode)?;
    let take = (n as usize).min(bytes.len());
    out.write_all(&bytes[..take])?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(dir: &tempfile::TempDir, name: &str, content: &[u8]) -> std::path::PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    /// Requesting exactly as many (or more) lines than exist in a file with
    /// *no* trailing newline must not add a terminator after the last line:
    /// `is_last_selected` is true and `take == all_lines.len()`, so every
    /// term of `!is_last_selected || take < all_lines.len() ||
    /// file_ends_with_newline` is false. Pins the `i + 1 == take` equality
    /// (against `!=` and against `*`, which would make `is_last_selected`
    /// unreachable), `take < all_lines.len()` (against `<=`), and `delete !`
    /// (which would make the first term `is_last_selected` itself, true
    /// here, wrongly forcing a terminator).
    #[test]
    fn run_lines_no_trailing_newline_all_requested_omits_final_terminator() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(&dir, "f.txt", b"aaa\nbbb\nccc");
        let mut out: Vec<u8> = Vec::new();
        run_lines(&p, 10, false, &mut out, crate::IoMode::Mmap, None).unwrap();
        assert_eq!(out, b"aaa\nbbb\nccc");
    }

    /// Requesting *fewer* lines than exist in a file with no trailing
    /// newline must still terminate the last *printed* line (more content
    /// follows it in the source file). Pins `take < all_lines.len()`
    /// against `==`/`>` and the `||` joining it to `!is_last_selected`
    /// against `&&` (here `!is_last_selected` is false, so only this term
    /// keeps the result true).
    #[test]
    fn run_lines_no_trailing_newline_partial_request_terminates_last_printed_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(&dir, "f.txt", b"aaa\nbbb\nccc");
        let mut out: Vec<u8> = Vec::new();
        run_lines(&p, 2, false, &mut out, crate::IoMode::Mmap, None).unwrap();
        assert_eq!(out, b"aaa\nbbb\n");
    }

    /// `file_len == 0 || n == 0` must short-circuit *before* attempting to
    /// read the file, because memory-mapping a genuinely empty file is
    /// platform-dependent (see the identical guard and comment in
    /// `run_lines`). Pins that `||` against a `&&` mutation: a non-empty
    /// file with `n == 0` requested must return cleanly with no output.
    #[test]
    fn run_bytes_zero_requested_on_nonempty_file_emits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(&dir, "f.bin", b"hello");
        let mut out: Vec<u8> = Vec::new();
        run_bytes(&p, 0, &mut out, crate::IoMode::Mmap).unwrap();
        assert!(out.is_empty());
    }

    /// An empty file with a nonzero byte count requested must return
    /// cleanly (no attempt to mmap/read the empty file). Pins the same
    /// `||` from the other side.
    #[test]
    fn run_bytes_nonzero_requested_on_empty_file_emits_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(&dir, "f.bin", b"");
        let mut out: Vec<u8> = Vec::new();
        run_bytes(&p, 5, &mut out, crate::IoMode::Mmap).unwrap();
        assert!(out.is_empty());
    }
}
