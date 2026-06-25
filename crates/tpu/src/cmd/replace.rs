// Copyright (c) 2026, Michael Grier

//! `tpu replace` — in-place regex replace on an encoding-aware, normalised
//! view of a file.
//!
//! The pattern is applied to a LF-only normalised view so callers never need
//! to account for CRLF in their patterns.  `\n` in patterns always matches
//! the LF byte used inside the normalised view.  Replacements are denormalised
//! back to the file's dominant line-ending before writing.  The result is
//! written atomically via a temp file; the original is renamed to `<file>.bak`.
//!
//! `--multiline` prepends `(?m)` to the pattern, making `^` / `$` match at
//! LF boundaries within the file rather than only at the start / end of the
//! entire content.
//!
//! `--diff` writes a unified text diff of the changes (in normalised/LF space)
//! to the provided writer after the file has been successfully updated.
//!
//! ## Replacement-string escapes
//!
//! This module operates on raw `&[u8]` replacement bytes.  Backslash-escape
//! decoding (`\n` → LF, `\t` → TAB, `\\` → `\`, `\xHH`, `\uXXXX`, …) is the
//! responsibility of the *caller* — the CLI front-end in `main.rs` performs
//! that decoding via [`crate::escape::decode_bytes`] unless the user passes
//! `--literal-replacement`.  By the time bytes reach [`run`] they should
//! already contain real LF/TAB/etc. bytes for any escapes the user wrote.
//! Capture-group references (`$0`, `$1`, `$name`, `$$`) are then expanded by
//! the regex engine via [`regex::Captures::expand`].
//!
//! ## Write-time mojibake guard
//!
//! After regex substitution and before any bytes touch disk, [`run`]
//! forwards the rewritten file content through
//! [`crate::mojibake::check_write_does_not_introduce_mojibake`] using
//! the original file's content as the baseline.  A replacement that
//! introduces *new* mojibake matches (any of the canonical Latin-1,
//! punctuation, box-drawing, NBSP, or double-encoded fingerprints) is
//! rejected and the file is left untouched.  Pre-existing matches are
//! ignored, so callers are never punished for damage they did not
//! cause.  Pass [`WritePolicy::permissive`] / `--allow-mojibake` /
//! `"allow_mojibake": true` to override.

use std::{io::Write, path::Path, sync::Arc};

use harrier::{
    denormalise::DenormaliseWriter,
    encoding::{LineEnding, SourceConfig},
    source::Source,
};
use regex::bytes::Regex;

use crate::{
    IoMode,
    mojibake::{WritePolicy, check_write_does_not_introduce_mojibake},
};

/// Decode a user-supplied replacement string into the raw bytes that will be
/// passed to [`run`].
///
/// When `literal` is `false` (the default at the CLI), backslash escapes such
/// as `\n`, `\t`, `\\`, `\xHH`, `\uXXXX`, and `\UXXXXXXXX` are interpreted via
/// [`crate::escape::decode_bytes`] so users can write `\n` and get a real
/// newline (matching `sed` / `perl` / `ripgrep` semantics).  Capture-group
/// references (`$0`, `$1`, `$name`, `$$`) are *not* touched here — they are
/// expanded later by the regex engine.
///
/// When `literal` is `true`, the input string is passed through verbatim as
/// UTF-8 bytes; no escape decoding is performed and `\n` remains the
/// two-character sequence backslash + `n`.
///
/// Errors are returned as `String` so the CLI can surface them directly with
/// a `replace:` prefix.
pub fn decode_replacement(s: &str, literal: bool) -> Result<Vec<u8>, String> {
    if literal {
        Ok(s.as_bytes().to_vec())
    } else {
        crate::escape::decode_bytes(s).map_err(|e| format!("invalid escape in replacement: {e}"))
    }
}

/// Run the `replace` subcommand.
///
/// Returns the number of substitutions made.
///
/// When `multiline` is `true`, `(?m)` is prepended to `pattern` so that `^`
/// and `$` match at every LF boundary in the normalised view.  `\n` in
/// patterns always refers to the LF byte used internally; CRLF is
/// transparent.
///
/// When `line_ending_override` is `Some`, the specified ending is used for
/// denormalisation of replacement bytes instead of the file's detected
/// dominant ending.  The file's content encoding is still detected and
/// preserved.
///
/// When `diff_out` is `Some`, a unified text diff of the changes (computed in
/// LF-normalised space) is written to the provided writer after the file has
/// been successfully updated.
///
/// When `count_only` is `true`, the file is not modified; the return value is
/// the number of matches.
///
/// When `dry_run` is `true`, the substitution is computed in memory and the
/// diff (if any) is written to `diff_out`, but the file is not modified.  The
/// caller is responsible for converting the return value to an exit code (exit
/// 1 when the count is > 0, exit 0 when it is 0).
///
/// `policy` controls the write-time mojibake guard.  By default the post-
/// substitution content is rejected if it introduces mojibake matches not
/// present in the file's prior decoded content; pass
/// [`WritePolicy::permissive`] (or the CLI's `--allow-mojibake`) to skip
/// the check.
#[allow(clippy::too_many_arguments)]
pub fn run(
    file: &Path,
    pattern: &str,
    replacement: &[u8],
    multiline: bool,
    fixed_strings: bool,
    line_ending_override: Option<LineEnding>,
    diff_out: Option<&mut dyn Write>,
    count_only: bool,
    dry_run: bool,
    io_mode: IoMode,
    policy: WritePolicy,
) -> Result<usize, Box<dyn std::error::Error>> {
    let escaped = if fixed_strings {
        regex::escape(pattern)
    } else {
        pattern.to_owned()
    };
    let effective_pattern = if multiline {
        format!("(?m){escaped}")
    } else {
        escaped
    };

    let branch = crate::open_as_branch(file, io_mode)?;
    let file_len = branch.byte_len();

    let source = Source::new(Arc::clone(&branch), SourceConfig::default())?;
    // Capture the file's encoding now (static ref; independent of source
    // lifetime) for use in the output line-ending post-processing step.
    let file_encoding = source.encoding();
    let line_ending = line_ending_override.unwrap_or_else(|| source.line_ending());
    let lines = source.as_lines()?;
    let view = lines.view_range(0..file_len)?;

    let re = Regex::new(&effective_pattern)?;

    // Snapshot the normalised old bytes now for diff computation later.
    // Only paid when --diff or --dry-run is requested.
    let old_norm: Option<Vec<u8>> = if diff_out.is_some() {
        Some(view.bytes.to_vec())
    } else {
        None
    };

    // Snapshot the raw old bytes for the mojibake write-time guard.  Only
    // taken when the guard is active and we will actually write (i.e. not
    // count-only).  Decoded against `file_encoding` later, after the
    // substitution result is known.
    let guard_old_bytes: Option<Vec<u8>> = if policy.reject_introduced_mojibake && !count_only {
        Some(view.bytes.to_vec())
    } else {
        None
    };

    // A splice descriptor: source-coordinate range and its denormalised
    // replacement bytes.
    struct Splice {
        source_start: u64,
        source_len: u64,
        content: Vec<u8>,
    }

    // Collect all matches as source-coordinate splices.
    let mut splices: Vec<Splice> = re
        .captures_iter(&view.bytes)
        .map(|caps| {
            let m = caps.get(0).unwrap();
            let source_start =
                view.byte_range_start() + view.offset_map.to_source(m.start() as u64);
            let source_end = view.byte_range_start() + view.offset_map.to_source(m.end() as u64);
            let source_len = source_end - source_start;

            // Expand capture-group back-references in normalised space.
            let mut norm_repl: Vec<u8> = Vec::new();
            caps.expand(replacement, &mut norm_repl);

            // Denormalise: restore the file's dominant line terminator.
            let content = denormalize_bytes(&norm_repl, line_ending);

            Splice {
                source_start,
                source_len,
                content,
            }
        })
        .collect();

    let replacement_count = splices.len();

    // --count: return match count without applying any edits.
    if count_only {
        return Ok(replacement_count);
    }

    // Apply splices in reverse source order so earlier-offset splices can
    // reuse the original b1 source coordinates without adjustment.
    let b2 = branch.fork();
    splices.sort_unstable_by_key(|s| std::cmp::Reverse(s.source_start));
    for s in &splices {
        b2.splice(s.source_start, s.source_len, &s.content)?;
    }

    let out_bytes = redwing::materialize(&*b2)?;

    // Release all branch and view handles before any file-system work.
    // On Windows a memory-mapped file cannot be renamed while a mapping is open.
    drop(splices);
    drop(view);
    drop(lines);
    drop(b2);
    drop(branch);

    // When `--line-ending` is set, the splice step above already used the
    // override for replacement bytes, but the un-replaced regions still carry
    // the original file's line terminators.  Normalise the entire output now
    // so every line ending matches the requested convention.
    let out_bytes = match line_ending_override {
        None => out_bytes,
        Some(target) => apply_line_ending_to_all(out_bytes, file_encoding, target),
    };

    // Write atomically: temp file in same dir → rename original to .bak →
    // persist temp to original path.  Skipped for --dry-run.
    if !dry_run {
        // Mojibake write-time guard.  Decode old + new bytes via the
        // file's encoding so the comparison is in UTF-8 char space.
        if let Some(old_raw) = guard_old_bytes.as_deref() {
            let (old_text, _, _) = file_encoding.decode(old_raw);
            let (new_text, _, _) = file_encoding.decode(&out_bytes);
            check_write_does_not_introduce_mojibake(&old_text, &new_text)
                .map_err(|e| format!("replace: {}: {e}", file.display()))?;
        }

        // Atomic write via the shared temp→.bak→persist→restore helper.
        crate::atomic_write(file, &out_bytes)?;
    }

    // Emit the diff (for both --diff after a successful write and --dry-run).
    if let (Some(out), Some(old)) = (diff_out, old_norm) {
        let new_norm = re.replace_all(&old, replacement).into_owned();
        emit_unified_diff(file, &old, &new_norm, out)?;
    }

    Ok(replacement_count)
}

/// Write a unified text diff of `old_norm` → `new_norm` (both LF-normalised)
/// to `out`, using `file` as the path label in the diff header.
fn emit_unified_diff(
    file: &Path,
    old_norm: &[u8],
    new_norm: &[u8],
    out: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let old_str = String::from_utf8_lossy(old_norm);
    let new_str = String::from_utf8_lossy(new_norm);
    let label = file.to_string_lossy();

    let diff = similar::TextDiff::from_lines(old_str.as_ref(), new_str.as_ref());
    let text = diff
        .unified_diff()
        .header(&format!("a/{label}"), &format!("b/{label}"))
        .to_string();
    out.write_all(text.as_bytes())?;
    Ok(())
}

/// Expand a normalised (LF-only) byte slice to use the file's dominant line
/// terminator.
///
/// Each `\n` byte is passed through [`DenormaliseWriter`] backed by an
/// infinite repeat of `le`.  Non-newline bytes pass through unchanged.
fn denormalize_bytes(norm: &[u8], le: LineEnding) -> Vec<u8> {
    let mut dw = DenormaliseWriter::new(Vec::with_capacity(norm.len()), std::iter::repeat(le));
    // Vec<u8> never returns I/O errors; unwrap is safe.
    std::io::Write::write_all(&mut dw, norm).unwrap();
    // into_inner rather than finish: the infinite repeat iterator has no
    // surplus terminators to flush.
    dw.into_inner()
}

// ── Whole-file line-ending conversion (for --line-ending override) ────────────
//
// When `line_ending_override` is Some, the spliced output still contains the
// original file's line terminators in un-replaced regions.  The functions below
// normalise the entire materialized output to the requested line ending in a
// second pass.
//
// This works entirely in the file's *native* byte space — it never decodes to
// UTF-8 and re-encodes.  That matters for two reasons:
//   * encoding_rs has no UTF-16 *encoder*: `Encoding::encode` silently falls
//     back to UTF-8 for UTF-16LE/BE, which would corrupt the stream.
//   * decode()/encode() round-trips strip (and fail to re-add) a leading BOM.
// Operating on native bytes leaves the BOM and every non-newline byte
// untouched, and keeps UTF-16 code units correctly aligned.
//
// Encoding-specific line-ending code units:
//   UTF-16LE  CR = [0x0D, 0x00]  LF = [0x0A, 0x00]  CRLF = [0x0D,0x00,0x0A,0x00]
//   UTF-16BE  CR = [0x00, 0x0D]  LF = [0x00, 0x0A]  CRLF = [0x00,0x0D,0x00,0x0A]
//   All else  CR = [0x0D]        LF = [0x0A]        CRLF = [0x0D, 0x0A]

/// Rewrite every line ending in `bytes` (in the file's native `encoding`) to
/// `target`, leaving the BOM and all non-newline bytes untouched.
///
/// The input is first normalised to LF-only in native byte space, then the
/// `target` ending is applied.  No decode/re-encode round-trip is performed.
fn apply_line_ending_to_all(
    bytes: Vec<u8>,
    encoding: &'static encoding_rs::Encoding,
    target: LineEnding,
) -> Vec<u8> {
    if encoding == encoding_rs::UTF_16LE {
        let lf = normalize_u16_to_lf(&bytes, [0x0D, 0x00], [0x0A, 0x00]);
        match target {
            LineEnding::Lf => lf,
            LineEnding::CrLf => le_replace_pairs(&lf, [0x0A, 0x00], &[0x0D, 0x00, 0x0A, 0x00]),
            LineEnding::Cr => le_replace_pairs(&lf, [0x0A, 0x00], &[0x0D, 0x00]),
        }
    } else if encoding == encoding_rs::UTF_16BE {
        let lf = normalize_u16_to_lf(&bytes, [0x00, 0x0D], [0x00, 0x0A]);
        match target {
            LineEnding::Lf => lf,
            LineEnding::CrLf => le_replace_pairs(&lf, [0x00, 0x0A], &[0x00, 0x0D, 0x00, 0x0A]),
            LineEnding::Cr => le_replace_pairs(&lf, [0x00, 0x0A], &[0x00, 0x0D]),
        }
    } else {
        let lf = normalize_bytes_to_lf(&bytes);
        match target {
            LineEnding::Lf => lf,
            LineEnding::CrLf => le_insert_cr_before_lf(&lf),
            LineEnding::Cr => lf
                .iter()
                .map(|&b| if b == 0x0A { 0x0D } else { b })
                .collect(),
        }
    }
}

/// Normalise CRLF and lone-CR endings to LF in a single-byte / UTF-8 stream.
///
/// `0x0D` / `0x0A` are unambiguous line-ending bytes in every non-UTF-16
/// encoding harrier detects (UTF-8 continuation bytes and Shift-JIS trailing
/// bytes never take those values), so a byte-level scan is safe.
fn normalize_bytes_to_lf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x0D {
            // CR (lone) or CRLF → LF.
            out.push(0x0A);
            if i + 1 < bytes.len() && bytes[i + 1] == 0x0A {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Normalise CRLF and lone-CR endings to LF in a UTF-16 stream, given the
/// 2-byte `cr` and `lf` code units in the correct byte order.
///
/// Scans in 2-byte (code-unit) steps so a `0x0D` / `0x0A` byte that happens to
/// be one half of an unrelated code unit (e.g. `U+0D00`) is never mistaken for
/// a line terminator.
fn normalize_u16_to_lf(bytes: &[u8], cr: [u8; 2], lf: [u8; 2]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == cr[0] && bytes[i + 1] == cr[1] {
            // CR unit (lone) or CRLF → LF unit.
            out.extend_from_slice(&lf);
            if i + 3 < bytes.len() && bytes[i + 2] == lf[0] && bytes[i + 3] == lf[1] {
                i += 4;
            } else {
                i += 2;
            }
        } else {
            out.push(bytes[i]);
            out.push(bytes[i + 1]);
            i += 2;
        }
    }
    if i < bytes.len() {
        // Odd trailing byte (should not occur in valid UTF-16); forward it.
        out.push(bytes[i]);
    }
    out
}

/// Scan `bytes` in 2-byte steps, replacing every occurrence of `needle`
/// (a UTF-16 code unit) with `replacement`.
fn le_replace_pairs(bytes: &[u8], needle: [u8; 2], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 20);
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == needle[0] && bytes[i + 1] == needle[1] {
            out.extend_from_slice(replacement);
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    if i < bytes.len() {
        out.push(bytes[i]);
    }
    out
}

/// Insert `\r` (0x0D) before each `\n` (0x0A) byte in a single-byte encoding.
fn le_insert_cr_before_lf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 20);
    for &b in bytes {
        if b == 0x0A {
            out.push(0x0D);
        }
        out.push(b);
    }
    out
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::{fs, io::Write as IoWrite};

    use tempfile::NamedTempFile;

    use super::*;

    // ── decode_replacement ──────────────────────────────────────────────────

    #[test]
    fn decode_replacement_default_decodes_backslash_n_to_lf() {
        assert_eq!(decode_replacement("a\\nb", false).unwrap(), b"a\nb");
    }

    #[test]
    fn decode_replacement_default_decodes_backslash_t_to_tab() {
        assert_eq!(decode_replacement("a\\tb", false).unwrap(), b"a\tb");
    }

    #[test]
    fn decode_replacement_default_decodes_double_backslash() {
        assert_eq!(decode_replacement("a\\\\b", false).unwrap(), b"a\\b");
    }

    #[test]
    fn decode_replacement_default_decodes_hex_escape() {
        // \x41 == 'A'
        assert_eq!(decode_replacement("\\x41", false).unwrap(), b"A");
    }

    #[test]
    fn decode_replacement_default_decodes_unicode_escape() {
        // \u00e9 == 'é' (U+00E9), encoded as 0xC3 0xA9 in UTF-8.
        assert_eq!(decode_replacement("\\u00e9", false).unwrap(), b"\xC3\xA9");
    }

    #[test]
    fn decode_replacement_default_passes_through_plain_ascii() {
        assert_eq!(decode_replacement("hello", false).unwrap(), b"hello");
    }

    #[test]
    fn decode_replacement_default_does_not_touch_dollar_capture_refs() {
        // Capture group expansion happens later in the regex engine; at this
        // layer `$1` and `$$` must be passed through verbatim.
        assert_eq!(decode_replacement("$1-$2", false).unwrap(), b"$1-$2");
        assert_eq!(decode_replacement("$$", false).unwrap(), b"$$");
    }

    #[test]
    fn decode_replacement_default_rejects_unknown_escape() {
        let err = decode_replacement("a\\qb", false).unwrap_err();
        assert!(
            err.contains("invalid escape in replacement"),
            "error should be prefixed for the CLI; got {err:?}"
        );
    }

    #[test]
    fn decode_replacement_literal_keeps_backslash_n_verbatim() {
        // In literal mode the two-character sequence `\` + `n` survives.
        assert_eq!(decode_replacement("a\\nb", true).unwrap(), b"a\\nb");
    }

    #[test]
    fn decode_replacement_literal_accepts_unknown_escape() {
        // Literal mode performs no decoding, so what would otherwise be an
        // unknown escape is just passed through.
        assert_eq!(decode_replacement("a\\qb", true).unwrap(), b"a\\qb");
    }

    #[test]
    fn decode_replacement_literal_passes_through_real_newline() {
        // A real newline byte in the input string (not a `\n` escape) is
        // preserved in both modes.
        assert_eq!(decode_replacement("a\nb", true).unwrap(), b"a\nb");
        assert_eq!(decode_replacement("a\nb", false).unwrap(), b"a\nb");
    }

    #[test]
    fn decode_replacement_default_decodes_long_unicode_escape() {
        // \U0001F600 == U+1F600 (😀), encoded as 0xF0 0x9F 0x98 0x80 in UTF-8.
        assert_eq!(
            decode_replacement("\\U0001F600", false).unwrap(),
            b"\xF0\x9F\x98\x80"
        );
    }

    // ── run() integration tests ─────────────────────────────────────────────

    /// Test-only wrapper that forwards to `run()` with `IoMode::Mmap`.
    #[allow(clippy::too_many_arguments)]
    fn run_test(
        file: &Path,
        pattern: &str,
        replacement: &[u8],
        multiline: bool,
        fixed_strings: bool,
        line_ending_override: Option<LineEnding>,
        diff_out: Option<&mut dyn Write>,
        count: bool,
        dry_run: bool,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        run(
            file,
            pattern,
            replacement,
            multiline,
            fixed_strings,
            line_ending_override,
            diff_out,
            count,
            dry_run,
            IoMode::Mmap,
            crate::mojibake::WritePolicy::permissive(),
        )
    }

    /// Write `content` to a temp file, run `replace::run`, and return the
    /// resulting file bytes.  `diff` output is captured and returned separately.
    fn replace_file(
        content: &[u8],
        pattern: &str,
        replacement: &[u8],
        multiline: bool,
        want_diff: bool,
    ) -> (Vec<u8>, String) {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        let path = f.path().to_path_buf();
        // NamedTempFile keeps the file alive via its handle; persist a copy
        // at the same path so the bak rename can succeed.
        drop(f);

        // Re-create the file now that the handle is dropped (temp file was
        // already persisted at path; we need it to still exist).
        fs::write(&path, content).unwrap();

        let mut diff_buf: Vec<u8> = Vec::new();
        let diff_out: Option<&mut dyn Write> = if want_diff { Some(&mut diff_buf) } else { None };

        run_test(
            &path,
            pattern,
            replacement,
            multiline,
            false,
            None,
            diff_out,
            false,
            false,
        )
        .unwrap();

        let result = fs::read(&path).unwrap();
        // Clean up .bak
        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&bak);
        let _ = fs::remove_file(&path);

        (result, String::from_utf8_lossy(&diff_buf).into_owned())
    }

    // ── Normal cases ──────────────────────────────────────────────────────────

    #[test]
    fn replace_simple_word() {
        let (out, _) = replace_file(b"hello world\n", "world", b"Rust", false, false);
        assert_eq!(out, b"hello Rust\n");
    }

    #[test]
    fn replace_no_matches() {
        let (out, _) = replace_file(b"hello world\n", "xyz", b"ZZZ", false, false);
        assert_eq!(out, b"hello world\n");
    }

    #[test]
    fn replace_multiple_occurrences() {
        let (out, _) = replace_file(b"aaa\n", "a", b"b", false, false);
        assert_eq!(out, b"bbb\n");
    }

    #[test]
    fn replace_with_capture_group() {
        let (out, _) = replace_file(
            b"2026-03-07\n",
            r"(\d{4})-(\d{2})-(\d{2})",
            b"$3/$2/$1",
            false,
            false,
        );
        assert_eq!(out, b"07/03/2026\n");
    }

    #[test]
    fn replace_named_capture_group() {
        let (out, _) = replace_file(
            b"key=value\n",
            r"(?P<k>\w+)=(?P<v>\w+)",
            b"$v=$k",
            false,
            false,
        );
        assert_eq!(out, b"value=key\n");
    }

    #[test]
    fn replace_multiline() {
        // Without (?m), ^ matches only at start of input.
        let (out, _) = replace_file(b"line1\nline2\nline3\n", "^line", b"item", false, false);
        assert_eq!(out, b"item1\nline2\nline3\n");
    }

    #[test]
    fn replace_multiline_start_anchor() {
        // With (?m) / --multiline, ^ matches at start of every line.
        let (out, _) = replace_file(b"line1\nline2\nline3\n", "^line", b"item", true, false);
        assert_eq!(out, b"item1\nitem2\nitem3\n");
    }

    #[test]
    fn replace_multiline_end_anchor() {
        let (out, _) = replace_file(b"foo1\nfoo2\n", r"\d$", b"X", true, false);
        assert_eq!(out, b"fooX\nfooX\n");
    }

    #[test]
    fn replace_multiline_whole_line() {
        // (?m)^.+$ matches each non-empty line body (without the newline).
        // ^.*$ is intentionally avoided: it also matches the zero-length
        // empty string that the regex engine sees after a trailing newline.
        let (out, _) = replace_file(b"alpha\nbeta\n", "^.+$", b"X", true, false);
        assert_eq!(out, b"X\nX\n");
    }

    #[test]
    fn replace_crlf_file_transparent() {
        // CRLF file: pattern uses \n (LF) because harrier normalises.
        let content = b"line1\r\nline2\r\n";
        let (out, _) = replace_file(content, "line1", b"item1", false, false);
        assert_eq!(out, b"item1\r\nline2\r\n");
    }

    #[test]
    fn replace_crlf_multiline_anchor() {
        // (?m) anchors work through CRLF normalisation transparently.
        let content = b"one\r\ntwo\r\n";
        let (out, _) = replace_file(content, "^two", b"TWO", true, false);
        assert_eq!(out, b"one\r\nTWO\r\n");
    }

    /// Encode `text` (LF-only) as UTF-16LE with a BOM, mapping each `\n` to the
    /// `ending` byte sequence, then run `replace` with `line_ending_override`
    /// and return the resulting raw file bytes.
    fn replace_utf16le_with_override(
        text_lf: &str,
        ending: &str,
        pattern: &str,
        replacement: &[u8],
        line_ending_override: Option<LineEnding>,
    ) -> Vec<u8> {
        let native = text_lf.replace('\n', ending);
        let mut bytes: Vec<u8> = vec![0xFF, 0xFE]; // UTF-16LE BOM
        for unit in native.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }

        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&bytes).unwrap();
        f.flush().unwrap();
        let path = f.path().to_path_buf();
        drop(f);
        fs::write(&path, &bytes).unwrap();

        run_test(
            &path,
            pattern,
            replacement,
            false,
            false,
            line_ending_override,
            None,
            false,
            false,
        )
        .unwrap();

        let result = fs::read(&path).unwrap();
        let _ = fs::remove_file(format!("{}.bak", path.display()));
        let _ = fs::remove_file(&path);
        result
    }

    #[test]
    fn replace_utf16le_line_ending_crlf_to_lf() {
        // Regression for the double line-ending pass.  The old, non-encoding-
        // aware first pass stripped *every* 0x0D byte regardless of code-unit
        // alignment — including the 0x0D of each `[0x0D,0x00]` CR unit — which
        // misaligned the whole UTF-16 stream before the second pass decoded it.
        //
        // `--line-ending` rewrites *all* endings whole-file even when the
        // pattern matches nothing (matching is incidental here; note that
        // `replace` matches against the file's native bytes, so an ASCII
        // pattern does not match UTF-16-encoded text anyway).  The output must
        // still decode cleanly, keep its BOM, and carry LF-only endings.
        let out = replace_utf16le_with_override(
            "foo\nbar\n",
            "\r\n",
            "no-such-match",
            b"",
            Some(LineEnding::Lf),
        );
        // Result is valid UTF-16LE (even length, BOM preserved).
        assert_eq!(out.len() % 2, 0, "UTF-16LE byte length must stay even");
        assert_eq!(&out[..2], &[0xFF, 0xFE], "BOM must be preserved");
        let (decoded, _, had_errors) = encoding_rs::UTF_16LE.decode(&out);
        assert!(!had_errors, "decoded with replacement chars: {decoded:?}");
        assert_eq!(decoded, "foo\nbar\n");
        assert!(!decoded.contains('\r'), "CRLF was not converted to LF");
    }

    #[test]
    fn replace_utf16le_line_ending_lf_to_crlf() {
        let out = replace_utf16le_with_override(
            "foo\nbar\n",
            "\n",
            "no-such-match",
            b"",
            Some(LineEnding::CrLf),
        );
        assert_eq!(out.len() % 2, 0, "UTF-16LE byte length must stay even");
        assert_eq!(&out[..2], &[0xFF, 0xFE], "BOM must be preserved");
        let (decoded, _, had_errors) = encoding_rs::UTF_16LE.decode(&out);
        assert!(!had_errors, "decoded with replacement chars: {decoded:?}");
        assert_eq!(decoded, "foo\r\nbar\r\n");
    }

    #[test]
    fn replace_back_reference_zero() {
        // $0 expands to the whole match.
        let (out, _) = replace_file(b"hello\n", "ell", b"[$0]", false, false);
        assert_eq!(out, b"h[ell]o\n");
    }

    #[test]
    fn replace_literal_dollar() {
        // $$ in replacement template becomes a literal $.
        let (out, _) = replace_file(b"price 10\n", r"\d+", b"$$$$5", false, false);
        assert_eq!(out, b"price $$5\n");
    }

    #[test]
    fn replace_preserves_bak() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"original\n").unwrap();
        f.flush().unwrap();
        let path = f.path().to_path_buf();
        drop(f);
        fs::write(&path, b"original\n").unwrap();

        run_test(
            &path,
            "original",
            b"replaced",
            false,
            false,
            None,
            None,
            false,
            false,
        )
        .unwrap();

        let bak = format!("{}.bak", path.display());
        let bak_bytes = fs::read(&bak).unwrap();
        assert_eq!(bak_bytes, b"original\n");

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }

    // ── Diff output ───────────────────────────────────────────────────────────

    #[test]
    fn diff_no_changes_empty_diff_body() {
        let (_, diff) = replace_file(b"unchanged\n", "xyz", b"abc", false, true);
        // No hunks when nothing changed.
        assert!(!diff.contains("@@"), "expected no diff hunks: {diff:?}");
    }

    #[test]
    fn diff_shows_changed_line() {
        let (_, diff) = replace_file(b"hello world\n", "world", b"Rust", false, true);
        assert!(
            diff.contains("-hello world"),
            "missing removed line: {diff:?}"
        );
        assert!(diff.contains("+hello Rust"), "missing added line: {diff:?}");
    }

    #[test]
    fn diff_header_contains_filename() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"old\n").unwrap();
        f.flush().unwrap();
        let path = f.path().to_path_buf();
        drop(f);
        fs::write(&path, b"old\n").unwrap();

        let mut diff_buf: Vec<u8> = Vec::new();
        run_test(
            &path,
            "old",
            b"new",
            false,
            false,
            None,
            Some(&mut diff_buf),
            false,
            false,
        )
        .unwrap();

        let diff = String::from_utf8_lossy(&diff_buf);
        assert!(diff.contains("a/"), "missing a/ prefix: {diff:?}");
        assert!(diff.contains("b/"), "missing b/ prefix: {diff:?}");

        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&bak);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn diff_multiline_replace() {
        let input = b"alpha\nbeta\ngamma\n";
        let (_, diff) = replace_file(input, "^beta$", b"BETA", true, true);
        assert!(diff.contains("-beta"), "missing removed line: {diff:?}");
        assert!(diff.contains("+BETA"), "missing added line: {diff:?}");
        assert!(
            !diff.contains("-alpha"),
            "alpha should not appear in diff: {diff:?}"
        );
    }

    #[test]
    fn diff_multiple_hunks() {
        // Changes to first and last line, context line in between.
        let input = b"AAA\ncontext\nBBB\n";
        let (_, diff) = replace_file(input, "AAA|BBB", b"X", false, true);
        assert!(diff.contains("-AAA"), "{diff:?}");
        assert!(diff.contains("+X"), "{diff:?}");
        assert!(diff.contains("-BBB"), "{diff:?}");
    }
}
