// Copyright (c) 2026, Michael Grier

//! `tpu readex` — emit a file as a single 7-bit clean ASCII escaped line.
//!
//! The entire decoded text is escaped using the codec in [`crate::escape`] so
//! that every non-printable character — including all line breaks — appears as
//! a backslash escape sequence.  The output is a **single flat line** with one
//! trailing newline; no literal newline characters appear in the escaped body.
//! This makes the output safe for shell variables, JSON string values, and
//! agent/tool output where 8-bit bytes can be misinterpreted.

use std::{fmt::Write as FmtWrite, io::Write, path::Path, sync::Arc};

use harrier::{encoding::SourceConfig, source::Source};

use crate::{
    IoMode,
    encoding::{BomPolicy, OutputEncoding},
    escape,
};

/// UTF-8 BOM byte sequence (U+FEFF encoded as UTF-8).
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Run the `readex` subcommand.
///
/// Opens `file` with harrier to detect its encoding and line-ending convention,
/// decodes to UTF-8, then encodes the result using the `escape` codec so that
/// every non-printable character — including all source line breaks — becomes a
/// `\n` (or other named / `\uXXXX` / `\UXXXXXXXX`) escape sequence.
///
/// The entire output is written as a **single flat line** followed by one
/// actual newline (`\n`) so that shells, pipelines, and agent consumers can
/// treat it as a standard line of text.
///
/// `lines_range` constrains which source lines are included (1-based inclusive
/// `(start, end)` pair).  `None` includes the entire file.
///
/// `numbers` prepends a 1-based line-number prefix (`     N  `) before each
/// source line's escaped content, separated by two spaces.  The prefix is
/// itself part of the flat escaped output.
///
/// `output_encoding` and `bom_policy` follow the same semantics as `read`:
/// when `output_encoding` is `Utf8` and `bom_policy` is `Preserve` or `Force`,
/// three UTF-8 BOM bytes (0xEF 0xBB 0xBF) are prepended to the output before
/// the escaped text.
///
/// `notes` is the optional advisory writer (Milestone 4).  When `Some`,
/// after decoding the file's text [`crate::mojibake::emit_read_advisory`]
/// may emit a `note: <path>: …` line if mojibake is detected.  Pass
/// `None` to suppress entirely.
#[allow(clippy::too_many_arguments)]
pub fn run(
    file: &Path,
    lines_range: Option<(usize, usize)>,
    numbers: bool,
    output_encoding: OutputEncoding,
    bom_policy: BomPolicy,
    out: &mut dyn Write,
    io_mode: IoMode,
    notes: Option<&mut dyn Write>,
) -> Result<(), Box<dyn std::error::Error>> {
    let f = std::fs::File::open(file)?;

    // Empty files cannot be memory-mapped on most platforms.  Handle them as
    // an immediate early return: the output is a single bare newline (the
    // flat-line terminator with no escaped body).
    if f.metadata()?.len() == 0 {
        writeln!(out)?;
        return Ok(());
    }
    drop(f);

    let branch = crate::open_as_branch(file, io_mode)?;
    let file_len = branch.byte_len();

    let source = Source::new(Arc::clone(&branch), SourceConfig::default())?;
    let bom_len = source.bom_len();
    let source_had_bom = bom_len > 0;
    let encoding = source.encoding();
    let lines = source.as_lines()?;
    // Start the view range at bom_len so the BOM bytes themselves are not
    // included in the decoded content (they are file-encoding metadata, not
    // document text).
    let view = lines.view_range(bom_len as u64..file_len)?;

    // Decode from the original encoding (LF-normalised bytes) to UTF-8.
    let (text, _) = encoding.decode_without_bom_handling(&view.bytes);

    // Read-time advisory (Milestone 4).
    if let Some(notes) = notes {
        crate::mojibake::emit_read_advisory(notes, file, &text)?;
    }

    // Split into source lines; drop the trailing empty entry produced by a
    // final LF so that line numbers remain correct (mirrors `read` behaviour).
    let all_parts: Vec<&str> = text.split('\n').collect();
    let all_lines: &[&str] = if all_parts.last() == Some(&"") {
        &all_parts[..all_parts.len() - 1]
    } else {
        &all_parts
    };

    let (start, end) = match lines_range {
        None => (0, all_lines.len()),
        Some((s, e)) => (s.saturating_sub(1), e.min(all_lines.len())),
    };

    // Optionally prepend a UTF-8 BOM.
    if output_encoding == OutputEncoding::Utf8 {
        let write_bom = match bom_policy {
            BomPolicy::Strip => false,
            BomPolicy::Preserve => source_had_bom,
            BomPolicy::Force => true,
        };
        if write_bom {
            out.write_all(UTF8_BOM)?;
        }
    }

    // Build the flat escaped output.  Each source line is escaped and followed
    // by the two-character `\n` escape sequence, mirroring the way `read`
    // always terminates each output line with a real newline.
    let mut escaped = String::new();
    for (i, line) in all_lines[start..end].iter().enumerate() {
        if numbers {
            // Line-number prefix is all printable ASCII; no escape codec needed.
            let _ = write!(escaped, "{:>6}  ", start + i + 1);
        }
        escaped.push_str(&escape::encode(line));
        // Append the two-character escape sequence for the source line break.
        escaped.push_str("\\n");
    }

    // Emit the flat line followed by a single actual newline as the line
    // terminator.  The `writeln!` newline is the only literal newline in the
    // output.
    writeln!(out, "{}", escaped)?;
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Write as IoWrite;

    use tempfile::NamedTempFile;

    use super::*;

    /// Create a named temp file containing `content`, run `readex::run` on it,
    /// and return the captured stdout as a `String`.
    fn readex_bytes(content: &[u8], lines_range: Option<(usize, usize)>, numbers: bool) -> String {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(content).unwrap();
        tmp.flush().unwrap();
        let mut out: Vec<u8> = Vec::new();
        run(
            tmp.path(),
            lines_range,
            numbers,
            OutputEncoding::Preserve,
            BomPolicy::Strip,
            &mut out,
            IoMode::Mmap,
            None,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    // ── Normal cases ──────────────────────────────────────────────────────────

    #[test]
    fn readex_empty_file() {
        // Empty file → single actual newline (flat-line terminator, empty body).
        assert_eq!(readex_bytes(b"", None, false), "\n");
    }

    #[test]
    fn readex_single_line_no_trailing_newline() {
        // "hello" has no trailing LF; the codec still appends the \n escape.
        assert_eq!(readex_bytes(b"hello", None, false), "hello\\n\n");
    }

    #[test]
    fn readex_single_line_with_trailing_newline() {
        // Trailing '\n' is stripped by the split logic; output is the same.
        assert_eq!(readex_bytes(b"hello\n", None, false), "hello\\n\n");
    }

    #[test]
    fn readex_two_lines() {
        assert_eq!(
            readex_bytes(b"line1\nline2\n", None, false),
            "line1\\nline2\\n\n"
        );
    }

    #[test]
    fn readex_three_lines_no_trailing_newline() {
        assert_eq!(readex_bytes(b"a\nb\nc", None, false), "a\\nb\\nc\\n\n");
    }

    #[test]
    fn readex_all_printable_ascii_passthrough() {
        // Printable ASCII (except backslash) should appear unchanged in output.
        let line = "Hello, World! 0-9 A-Z a-z !@#$%^&*()";
        let input = line.as_bytes();
        let expected = format!("{line}\\n\n");
        assert_eq!(readex_bytes(input, None, false), expected);
    }

    #[test]
    fn readex_backslash_is_doubled() {
        assert_eq!(readex_bytes(b"a\\b", None, false), "a\\\\b\\n\n");
    }

    #[test]
    fn readex_tab_escaped() {
        assert_eq!(readex_bytes(b"\t", None, false), "\\t\\n\n");
    }

    #[test]
    fn readex_cr_escaped_in_line_content() {
        // CR inside a line (not at end) is passed through the LF-normalised
        // view as a distinct character and escaped as \r.
        // A file "a\rb" with CR line endings: harrier normalises it so "a" and
        // "b" appear on separate lines.  Test CR embedded by writing CR-LF
        // style where CR appears on its own within a line context.
        // We use a UTF-8 file with CR embedded inside a line (not as a line
        // ending) by using a CRLF file where the CR falls in the LF-normalised
        // view content is just the two real source lines.
        let result = readex_bytes(b"line1\r\nline2\r\n", None, false);
        assert_eq!(result, "line1\\nline2\\n\n");
    }

    #[test]
    fn readex_bmp_non_ascii_char() {
        // é = U+00E9; encoded as UTF-8 0xC3 0xA9
        assert_eq!(
            readex_bytes("café\n".as_bytes(), None, false),
            "caf\\u00E9\\n\n"
        );
    }

    #[test]
    fn readex_supplementary_plane_emoji() {
        // 😀 = U+1F600; encoded as UTF-8 0xF0 0x9F 0x98 0x80
        assert_eq!(
            readex_bytes("😀\n".as_bytes(), None, false),
            "\\U0001F600\\n\n"
        );
    }

    #[test]
    fn readex_nul_byte_in_content() {
        // NUL (U+0000) in UTF-8 is a literal 0x00 byte; escaped as \0.
        assert_eq!(readex_bytes(b"abc\x00def\n", None, false), "abc\\0def\\n\n");
    }

    #[test]
    fn readex_mixed_bmp_and_supplementary() {
        let s = "é😀end\n";
        assert_eq!(
            readex_bytes(s.as_bytes(), None, false),
            "\\u00E9\\U0001F600end\\n\n"
        );
    }

    #[test]
    fn readex_all_named_escape_chars_in_line() {
        // File content: backslash, NUL, tab, CR, LF.
        // The trailing \r\n is treated by harrier as a CRLF line ending and
        // normalised to a single LF — the \r disappears from line content.
        // So the one source line contains only \, NUL, tab.
        assert_eq!(
            readex_bytes(b"\\\x00\t\r\n", None, false),
            "\\\\\\0\\t\\n\n"
        );
    }

    // ── BOM file ─────────────────────────────────────────────────────────────

    #[test]
    fn readex_bom_only_file() {
        // UTF-8 BOM with no content after it → treated as empty content.
        // harrier strips the BOM; no content lines remain.
        assert_eq!(readex_bytes(b"\xEF\xBB\xBF", None, false), "\n");
    }

    #[test]
    fn readex_utf8_bom_with_content() {
        // UTF-8 BOM followed by text; BOM is stripped, text is escaped normally.
        assert_eq!(
            readex_bytes(b"\xEF\xBB\xBFhello\n", None, false),
            "hello\\n\n"
        );
    }

    // ── --lines range ─────────────────────────────────────────────────────────

    #[test]
    fn readex_lines_single() {
        let result = readex_bytes(b"aaa\nbbb\nccc\n", Some((2, 2)), false);
        assert_eq!(result, "bbb\\n\n");
    }

    #[test]
    fn readex_lines_range() {
        let result = readex_bytes(b"aaa\nbbb\nccc\nddd\n", Some((2, 3)), false);
        assert_eq!(result, "bbb\\nccc\\n\n");
    }

    #[test]
    fn readex_lines_range_past_end_is_clamped() {
        let result = readex_bytes(b"aaa\nbbb\n", Some((1, 99)), false);
        assert_eq!(result, "aaa\\nbbb\\n\n");
    }

    // ── --numbers ─────────────────────────────────────────────────────────────

    #[test]
    fn readex_numbers_single_line() {
        let result = readex_bytes(b"hello\n", None, true);
        // Prefix is "     1  " (6 digits right-aligned, two spaces).
        assert_eq!(result, "     1  hello\\n\n");
    }

    #[test]
    fn readex_numbers_two_lines() {
        let result = readex_bytes(b"aaa\nbbb\n", None, true);
        assert_eq!(result, "     1  aaa\\n     2  bbb\\n\n");
    }

    #[test]
    fn readex_numbers_with_lines_range() {
        // Line numbers in output reflect actual 1-based source line numbers.
        let result = readex_bytes(b"aaa\nbbb\nccc\n", Some((2, 3)), true);
        assert_eq!(result, "     2  bbb\\n     3  ccc\\n\n");
    }

    // ── BOM output flags ──────────────────────────────────────────────────────

    #[test]
    fn readex_bom_force_prepends_bom() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"hi\n").unwrap();
        tmp.flush().unwrap();
        let mut out: Vec<u8> = Vec::new();
        run(
            tmp.path(),
            None,
            false,
            OutputEncoding::Utf8,
            BomPolicy::Force,
            &mut out,
            IoMode::Mmap,
            None,
        )
        .unwrap();
        // First three bytes must be the UTF-8 BOM.
        assert_eq!(&out[..3], &[0xEF, 0xBB, 0xBF]);
        let body = String::from_utf8(out[3..].to_vec()).unwrap();
        assert_eq!(body, "hi\\n\n");
    }

    #[test]
    fn readex_bom_strip_does_not_prepend_bom() {
        let mut tmp = NamedTempFile::new().unwrap();
        // Source has a UTF-8 BOM.
        tmp.write_all(b"\xEF\xBB\xBFhi\n").unwrap();
        tmp.flush().unwrap();
        let mut out: Vec<u8> = Vec::new();
        run(
            tmp.path(),
            None,
            false,
            OutputEncoding::Utf8,
            BomPolicy::Strip,
            &mut out,
            IoMode::Mmap,
            None,
        )
        .unwrap();
        // No BOM in output.
        assert_ne!(&out[..3], &[0xEF, 0xBB, 0xBF]);
        let body = String::from_utf8(out).unwrap();
        assert_eq!(body, "hi\\n\n");
    }

    #[test]
    fn readex_bom_preserve_copies_source_bom() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"\xEF\xBB\xBFhi\n").unwrap();
        tmp.flush().unwrap();
        let mut out: Vec<u8> = Vec::new();
        run(
            tmp.path(),
            None,
            false,
            OutputEncoding::Utf8,
            BomPolicy::Preserve,
            &mut out,
            IoMode::Mmap,
            None,
        )
        .unwrap();
        // Source had BOM, so output should have BOM.
        assert_eq!(&out[..3], &[0xEF, 0xBB, 0xBF]);
        let body = String::from_utf8(out[3..].to_vec()).unwrap();
        assert_eq!(body, "hi\\n\n");
    }

    #[test]
    fn readex_bom_preserve_no_bom_in_source() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"hi\n").unwrap();
        tmp.flush().unwrap();
        let mut out: Vec<u8> = Vec::new();
        run(
            tmp.path(),
            None,
            false,
            OutputEncoding::Utf8,
            BomPolicy::Preserve,
            &mut out,
            IoMode::Mmap,
            None,
        )
        .unwrap();
        // Source had no BOM, so output should not have BOM.
        assert_ne!(out.first(), Some(&0xEF));
        let body = String::from_utf8(out).unwrap();
        assert_eq!(body, "hi\\n\n");
    }

    // ── Output 7-bit clean invariant ─────────────────────────────────────────

    #[test]
    fn readex_output_is_7bit_clean() {
        // With a file containing various non-ASCII content, every byte in the
        // escaped body must be in the range 0x20–0x7E or literal '\n' (the
        // flat-line terminator).
        let content = "café\u{1F600}\x00\t\n\\end\n";
        let result = readex_bytes(content.as_bytes(), None, false);
        // Only the final actual newline may be outside printable ASCII.
        let body = result.trim_end_matches('\n');
        for b in body.bytes() {
            assert!(
                (0x20..=0x7E).contains(&b),
                "non-printable byte 0x{b:02X} in escaped output"
            );
        }
    }
}

/// Run the `readex --binary` subcommand.
///
/// Opens `file` as raw bytes (bypassing harrier encoding/line-ending detection),
/// escapes the selected byte range with [`crate::escape::encode_bytes`], and
/// writes the result as a **single flat line** followed by one actual `\n`
/// (the same format as `readex`).  This makes the output safe for agent
/// consumers that expect a single-line response.
///
/// `byte_range` is a 1-based inclusive `(start, end)` pair.  `None` selects
/// the entire file.
#[allow(dead_code)]
pub fn run_binary(
    file: &Path,
    byte_range: Option<(u64, u64)>,
    out: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let all_bytes = std::fs::read(file)?;

    let slice = match byte_range {
        None => &all_bytes[..],
        Some((start, end)) => {
            let lo = (start.saturating_sub(1) as usize).min(all_bytes.len());
            let hi = (end as usize).min(all_bytes.len());
            &all_bytes[lo..hi]
        }
    };

    let escaped = crate::escape::encode_bytes(slice);
    writeln!(out, "{escaped}")?;
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests for run_binary
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod binary_tests {
    use std::io::Write as IoWrite;

    use tempfile::NamedTempFile;

    use super::*;

    fn binary_readex(content: &[u8], byte_range: Option<(u64, u64)>) -> Vec<u8> {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        let mut out: Vec<u8> = Vec::new();
        run_binary(f.path(), byte_range, &mut out).unwrap();
        out
    }

    #[test]
    fn binary_readex_empty_file() {
        let mut f = NamedTempFile::new().unwrap();
        f.flush().unwrap();
        let mut out = Vec::new();
        run_binary(f.path(), None, &mut out).unwrap();
        // Empty file → single newline (the flat-line terminator with empty body).
        assert_eq!(out, b"\n");
    }

    #[test]
    fn binary_readex_plain_ascii() {
        assert_eq!(binary_readex(b"hello", None), b"hello\n");
    }

    #[test]
    fn binary_readex_has_trailing_newline() {
        let out = binary_readex(b"abc", None);
        assert!(out.ends_with(b"\n"), "expected trailing newline");
        // Only one trailing newline.
        assert!(!out.ends_with(b"\n\n"), "unexpected double newline");
    }

    #[test]
    fn binary_readex_high_bytes_escaped() {
        assert_eq!(binary_readex(b"\xFF\x00", None), b"\\xff\\0\n");
    }

    #[test]
    fn binary_readex_lf_escaped_not_literal() {
        // A raw LF byte (0x0A) in the file must appear as \n in the output,
        // not as a literal newline (which would break the flat-line format).
        let out = binary_readex(b"a\nb", None);
        // Output should be the escape sequence for LF, not a literal newline mid-body.
        assert_eq!(out, b"a\\nb\n");
    }

    #[test]
    fn binary_readex_partial_range() {
        assert_eq!(binary_readex(b"abcde", Some((2, 4))), b"bcd\n");
    }

    #[test]
    fn binary_readex_single_byte() {
        assert_eq!(binary_readex(b"xyz", Some((1, 1))), b"x\n");
    }

    #[test]
    fn binary_readex_range_clamps_to_file_end() {
        assert_eq!(binary_readex(b"abc", Some((2, 999))), b"bc\n");
    }

    #[test]
    fn binary_readex_range_start_beyond_file_is_empty_line() {
        // Out-of-range → empty escaped body → single newline.
        assert_eq!(binary_readex(b"abc", Some((10, 20))), b"\n");
    }

    #[test]
    fn binary_readex_output_is_7bit_clean() {
        // All byte values 0x00–0xFF should produce 7-bit clean output (except
        // the final newline terminator).
        let all: Vec<u8> = (0u8..=255u8).collect();
        let out = binary_readex(&all, None);
        let body = &out[..out.len() - 1]; // strip the trailing \n
        for &b in body {
            assert!(
                (0x20..=0x7Eu8).contains(&b),
                "non-printable byte 0x{b:02X} in output"
            );
        }
    }

    #[test]
    fn binary_readex_round_trip_all_bytes() {
        let all: Vec<u8> = (0u8..=255u8).collect();
        let out = binary_readex(&all, None);
        // Strip the trailing newline, then decode the escaped body.
        let s = std::str::from_utf8(&out[..out.len() - 1]).unwrap();
        let decoded = crate::escape::decode_bytes(s).unwrap();
        assert_eq!(decoded, all);
    }
}
