// Copyright (c) 2026, Michael Grier

//! `tpu tail` — emit the last N lines or N bytes of a file to stdout.

use std::{io::Write, path::Path, sync::Arc};

use harrier::{
    encoding::{LineEnding, SourceConfig},
    source::Source,
};

use crate::IoMode;

/// Selects how many lines or bytes to emit from the end of the file.
pub enum TailMode {
    /// Emit the last `n` logical lines.  Default n = 10.
    ///
    /// When `numbers` is true each output line is prefixed with its absolute
    /// 1-based file-line number and a tab; output always uses LF line endings.
    Lines { n: usize, numbers: bool },
    /// Emit the last `n` raw bytes verbatim.  No encoding or line-ending
    /// processing is applied.
    Bytes { n: u64 },
}

/// Run the `tail` subcommand.
///
/// Opens `file` and emits the last records selected by `mode` to `out`.
///
/// In `Lines` mode harrier detects the file encoding and line ending; output
/// is always UTF-8, re-emitted using the file's native terminator sequence.
/// A fixed-capacity ring buffer of size `n` is used to locate the last `n`
/// lines without buffering the full file content more than once.
///
/// In `Bytes` mode the raw byte stream is memory-mapped; the last `n` bytes
/// are written verbatim with no encoding or line-ending processing.
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
    mode: TailMode,
    out: &mut dyn Write,
    io_mode: IoMode,
    notes: Option<&mut dyn Write>,
) -> Result<(), Box<dyn std::error::Error>> {
    match mode {
        TailMode::Lines { n, numbers } => run_lines(file, n, numbers, out, io_mode, notes),
        TailMode::Bytes { n } => run_bytes(file, n, out, io_mode),
    }
}

/// Emit the last `n` lines of `file` to `out`, using the file's detected
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

    let branch = crate::open_as_branch(file, io_mode)?;
    let file_len = branch.byte_len();
    let source = Source::new(Arc::clone(&branch), SourceConfig::default())?;
    let bom_len = source.bom_len();
    let encoding = source.encoding();
    let line_ending = source.line_ending();
    let lines_iter = source.as_lines()?;
    // Skip BOM bytes so decoded text starts at the first content character.
    let view = lines_iter.view_range(bom_len as u64..file_len)?;
    let (text, _) = encoding.decode_without_bom_handling(&view.bytes);

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

    // Ring-buffer selection: the last min(n, total) lines.
    let start = all_lines.len().saturating_sub(n);
    let selected = &all_lines[start..];
    let take = selected.len();

    let terminator: &[u8] = match line_ending {
        LineEnding::Lf => b"\n",
        LineEnding::CrLf => b"\r\n",
        LineEnding::Cr => b"\r",
    };

    for (i, line) in selected.iter().enumerate() {
        if numbers {
            // Numbered mode: emit the absolute 1-based line number, always LF.
            writeln!(out, "{}\t{}", start + i + 1, line)?;
        } else {
            out.write_all(line.as_bytes())?;
            // The last selected line is always the last line of the entire file.
            // Emit its terminator only if the file actually ended with one.
            let is_last_selected = i + 1 == take;
            let had_terminator = !is_last_selected || file_ends_with_newline;
            if had_terminator {
                out.write_all(terminator)?;
            }
        }
    }

    Ok(())
}

/// Emit the last `n` raw bytes of `file` to `out` with no encoding or
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
    let branch = crate::open_as_branch(file, io_mode)?;
    let file_len = branch.byte_len();
    let start = file_len.saturating_sub(n);
    let read_len = file_len - start;
    let bytes = redwing::materialize_range(&*branch, start, read_len)?;
    out.write_all(&bytes)?;
    Ok(())
}
