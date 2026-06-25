// Copyright (c) 2026, Michael Grier

//! `tpu edit` — targeted in-place edits at known positions (line numbers or
//! byte offsets).
//!
//! See [`run`] for the full contract, coordinate model, and composability
//! invariant.
//!
//! ## Write-time mojibake guard
//!
//! Before any bytes touch disk, [`run`] forwards the *would-be* file
//! content through
//! [`crate::mojibake::check_write_does_not_introduce_mojibake`] using
//! the original file's content as the baseline.  A splice / insert /
//! delete whose result contains *new* mojibake matches (any of the
//! canonical Latin-1, punctuation, box-drawing, NBSP, or
//! double-encoded fingerprints) is rejected and the file is left
//! untouched.  Pre-existing matches are ignored.  Pass
//! [`WritePolicy::permissive`] / `--allow-mojibake` /
//! `"allow_mojibake": true` to override.

use harrier::{
    denormalise::DenormaliseWriter,
    encoding::{LineEnding, SourceConfig},
    source::Source,
    view::View,
};
use std::{fs, io::Write, path::Path, sync::Arc};

use crate::{
    IoMode,
    mojibake::{WritePolicy, check_write_does_not_introduce_mojibake},
};

/// Sentinel value parsed from `$` or `EOF` (case-insensitive) in any RANGE or
/// OFFSET argument to `tpu edit`.
///
/// - **Line mode**: resolves to the total line count of the file (last line).
///   For `--insert`, resolves to "append after the last line".
/// - **Binary mode**: resolves to the file's byte length (one past the last
///   byte), consistent with the `HashRangeEnd::Eof` sentinel in `read.rs`.
///
/// **Changing this constant value is a breaking change.**
pub const EOF_SENTINEL: usize = usize::MAX;

/// A single targeted edit operation.
///
/// All coordinates are expressed in **source byte offsets** in the original
/// file (before any edits in this invocation are applied).  Line-mode callers
/// must resolve 1-based line numbers to source byte ranges before constructing
/// `EditOp` values.
///
/// **Changing the discriminant values or field layout of this enum is a
/// breaking change.**
#[derive(Debug)]
pub enum EditOp {
    /// Remove bytes in `[start, end)` from the source.  `end` is exclusive.
    Delete { start: usize, end: usize },
    /// Insert `data` immediately before byte offset `offset` in the source.
    /// Equivalent to `Splice { start: offset, end: offset, data }`.
    Insert { offset: usize, data: Vec<u8> },
    /// Replace bytes in `[start, end)` with `data`.  `end` is exclusive.
    /// `data` may have any length (shorter, same, or longer).
    Splice {
        start: usize,
        end: usize,
        data: Vec<u8>,
    },
}

/// Execute a set of edit operations on `file`.
///
/// Callers must resolve all `ops` coordinates to source byte offsets **before**
/// calling this function.  For binary mode the CLI strings are 0-based byte
/// offsets; for line mode the caller uses [`line_range_to_source_bytes`] to
/// convert 1-based line numbers.
///
/// `--validate` pairs must be checked by the caller (via
/// `cmd::validate::run_all`) before calling this function.
///
/// # Composability invariant
///
/// All `ops` coordinates reference the **original file**.  `run` checks for
/// overlapping ranges, then applies ops in reverse start-offset order on a
/// forked redwing branch so each lower-address op sees its original position
/// undisturbed.
///
/// # Overlapping-patch policy
///
/// If any two ops have overlapping `[start, end)` ranges this function returns
/// an error immediately and the file is not modified.  Adjacent ops (end of
/// one == start of next) are permitted.
///
/// # Atomic write
///
/// temp file → rename original to `<file>.bak` → rename temp to original.
/// On failure after the `.bak` rename the `.bak` is renamed back.
///
/// # Returns
///
/// The number of ops applied.
pub fn run(
    file: &Path,
    ops: Vec<EditOp>,
    binary: bool,
    line_ending_override: Option<LineEnding>,
    diff_out: Option<&mut dyn Write>,
    io_mode: IoMode,
    policy: WritePolicy,
) -> Result<usize, Box<dyn std::error::Error>> {
    let _ = crate::recover_stranded_backup(file);
    if binary {
        let _ = diff_out; // binary mode: --diff is skipped (not meaningful in unified-diff format)
        // Binary mode operates on raw bytes that may not be valid UTF-8 in any
        // encoding; the mojibake guard is a text-level invariant and does not
        // apply.  Defer to caller's policy: silently skip.
        let _ = policy;
        run_binary(file, ops, io_mode)
    } else {
        run_line(file, ops, line_ending_override, diff_out, io_mode, policy)
    }
}

// ── Binary mode ───────────────────────────────────────────────────────────────

fn run_binary(
    file: &Path,
    ops: Vec<EditOp>,
    io_mode: IoMode,
) -> Result<usize, Box<dyn std::error::Error>> {
    if ops.is_empty() {
        return Ok(0);
    }

    // Normalise all ops to (start, end, data) tuples in source byte coords.
    struct Patch {
        start: usize,
        end: usize,
        data: Vec<u8>,
    }

    let mut patches: Vec<Patch> = ops
        .into_iter()
        .map(|op| match op {
            EditOp::Delete { start, end } => Patch {
                start,
                end,
                data: vec![],
            },
            EditOp::Insert { offset, data } => Patch {
                start: offset,
                end: offset,
                data,
            },
            EditOp::Splice { start, end, data } => Patch { start, end, data },
        })
        .collect();

    // Sort ascending by start for bounds + overlap checks.
    patches.sort_unstable_by_key(|p| p.start);

    // Bounds and overlap checks — all happen before any file I/O.
    let file_len = fs::metadata(file)
        .map_err(|e| format!("edit: cannot stat {}: {e}", file.display()))?
        .len() as usize;

    // Resolve EOF_SENTINEL to the actual file length.
    for p in &mut patches {
        if p.start == EOF_SENTINEL {
            p.start = file_len;
        }
        if p.end == EOF_SENTINEL {
            p.end = file_len;
        }
    }

    for p in &patches {
        if p.start > p.end {
            return Err(format!("edit: invalid range: start {} > end {}", p.start, p.end).into());
        }
        if p.end > file_len {
            return Err(format!(
                "edit: range [{}, {}) exceeds file length {}",
                p.start, p.end, file_len
            )
            .into());
        }
    }
    for w in patches.windows(2) {
        if w[0].end > w[1].start {
            return Err(format!(
                "edit: overlapping ranges [{}, {}) and [{}, {})",
                w[0].start, w[0].end, w[1].start, w[1].end
            )
            .into());
        }
    }

    let op_count = patches.len();

    // Open for read-only access.
    let branch = crate::open_as_branch(file, io_mode)?;
    let b2 = branch.fork();

    // Apply in reverse start-offset order so earlier coords remain valid.
    for p in patches.iter().rev() {
        let len = (p.end - p.start) as u64;
        b2.splice(p.start as u64, len, &p.data)?;
    }

    let out_bytes = redwing::materialize(&*b2)?;

    // Release all handles before file-system work — required on Windows
    // because a memory-mapped file cannot be renamed while mapped.
    drop(b2);
    drop(branch);

    // Atomic write via the shared temp→.bak→persist→restore helper.
    crate::atomic_write(file, &out_bytes)?;

    Ok(op_count)
}

// ── Line mode ────────────────────────────────────────────────────────────────

fn run_line(
    file: &Path,
    ops: Vec<EditOp>,
    line_ending_override: Option<LineEnding>,
    diff_out: Option<&mut dyn Write>,
    io_mode: IoMode,
    policy: WritePolicy,
) -> Result<usize, Box<dyn std::error::Error>> {
    if ops.is_empty() {
        return Ok(0);
    }

    let branch = crate::open_as_branch(file, io_mode)?;
    let file_len = branch.byte_len();

    let source = Source::new(Arc::clone(&branch), SourceConfig::default())?;
    let detected_ending = source.line_ending();
    let file_encoding = source.encoding();
    let line_ending = line_ending_override.unwrap_or(detected_ending);
    let bom_len = source.bom_len();
    let lines = source.as_lines()?; // consumes source
    let view = lines.view_range(bom_len as u64..file_len)?;

    // Normalise all ops to (source_start, source_end, denormalised_data).
    // Op start/end are 1-based line numbers at this stage.
    struct Patch {
        start: usize,
        end: usize,
        data: Vec<u8>,
    }

    // Count lines from the normalised view so the Insert arm can allow
    // offset == total_lines + 1 as a valid append position.
    let total_lines = {
        let b = &view.bytes;
        if b.is_empty() {
            0
        } else if b.last() == Some(&b'\n') {
            b.iter().filter(|&&c| c == b'\n').count()
        } else {
            b.iter().filter(|&&c| c == b'\n').count() + 1
        }
    };

    let mut patches: Vec<Patch> = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            EditOp::Delete { start, end } => {
                let (sb, eb) = line_range_to_source_bytes(&view, start, end)
                    .map_err(|e| format!("--delete: {e}"))?;
                patches.push(Patch {
                    start: sb,
                    end: eb,
                    data: vec![],
                });
            }
            EditOp::Insert { offset, data } => {
                // Insert before line `offset`: use the source-byte start of that line.
                // EOF_SENTINEL means append — insert at the very end of the file.
                // Offset == total_lines + 1 is also a valid append ("insert before a
                // hypothetical line just past the last one"), so treat it the same way.
                let sb = if offset == EOF_SENTINEL || offset == total_lines + 1 {
                    file_len as usize
                } else {
                    let (start, _) = line_range_to_source_bytes(&view, offset, offset)
                        .map_err(|e| format!("--insert: {e}"))?;
                    start
                };
                let dn = denorm_bytes(&data, line_ending);
                patches.push(Patch {
                    start: sb,
                    end: sb,
                    data: dn,
                });
            }
            EditOp::Splice { start, end, data } => {
                let (sb, eb) = line_range_to_source_bytes(&view, start, end)
                    .map_err(|e| format!("--splice: {e}"))?;
                let dn = denorm_bytes(&data, line_ending);
                patches.push(Patch {
                    start: sb,
                    end: eb,
                    data: dn,
                });
            }
        }
    }

    // Ascending sort for overlap check.
    patches.sort_unstable_by_key(|p| p.start);

    for w in patches.windows(2) {
        if w[0].end > w[1].start {
            return Err(format!(
                "edit: overlapping ranges [{}, {}) and [{}, {})",
                w[0].start, w[0].end, w[1].start, w[1].end
            )
            .into());
        }
    }

    let op_count = patches.len();

    // Snapshot the normalised old content for diff computation (cheap clone,
    // paid only when --diff is requested).
    let old_norm: Option<Vec<u8>> = if diff_out.is_some() {
        Some(view.bytes.to_vec())
    } else {
        None
    };

    // Snapshot the raw old bytes for the mojibake write-time guard.  Only
    // taken when the guard is active.  Decoded against `file_encoding`
    // after the splice result is known.
    let guard_old_bytes: Option<Vec<u8>> = if policy.reject_introduced_mojibake {
        Some(view.bytes.to_vec())
    } else {
        None
    };

    // Apply in reverse source order so earlier coord patches see original
    // positions undisturbed.
    let b2 = branch.fork();
    for p in patches.iter().rev() {
        let len = (p.end - p.start) as u64;
        b2.splice(p.start as u64, len, &p.data)?;
    }

    let out_bytes = redwing::materialize(&*b2)?;

    // Release all mmap-backed handles before file-system work (Windows requirement).
    drop(b2);
    drop(view);
    drop(lines);
    drop(branch);

    // Mojibake write-time guard.  Decode old + new bytes via the file's
    // encoding and compare in UTF-8 char space.
    if let Some(old_raw) = guard_old_bytes.as_deref() {
        let (old_text, _, _) = file_encoding.decode(old_raw);
        let (new_text, _, _) = file_encoding.decode(&out_bytes);
        check_write_does_not_introduce_mojibake(&old_text, &new_text)
            .map_err(|e| format!("edit: {}: {e}", file.display()))?;
    }

    // Atomic write via the shared temp→.bak→persist→restore helper.
    crate::atomic_write(file, &out_bytes)?;

    // Emit the unified text diff after a successful write.
    if let (Some(out), Some(old)) = (diff_out, old_norm) {
        let new_str_raw = String::from_utf8_lossy(&out_bytes[bom_len..]);
        let new_norm = new_str_raw.replace("\r\n", "\n").replace('\r', "\n");
        emit_unified_diff(file, &old, new_norm.as_bytes(), out)?;
    }

    Ok(op_count)
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

/// Expand normalised (LF-only) bytes to use line ending `le`.
fn denorm_bytes(norm: &[u8], le: LineEnding) -> Vec<u8> {
    let mut dw = DenormaliseWriter::new(Vec::with_capacity(norm.len()), std::iter::repeat(le));
    // Vec<u8> never returns I/O errors; unwrap is safe.
    std::io::Write::write_all(&mut dw, norm).unwrap();
    dw.into_inner()
}

// ── Line-coordinate helper ──────────────────────────────────────────────────────

/// Map a 1-based inclusive line range to source byte offsets `(start, end)`
/// where `end` is exclusive.
///
/// `view` must be a `harrier::view::View` built from the full file range
/// (or at least from the region that contains the requested lines).
/// Lines are counted in the normalised (LF-only) byte sequence: each `\n`
/// terminates a line, and a file that does not end with `\n` has a final
/// line without terminator.
///
/// Returned offsets are **branch-absolute source byte offsets** (i.e.,
/// they account for CRLF/CR source expansion and any BOM start offset
/// encoded in `view.byte_range_start()`).
///
/// # Errors
///
/// Returns an error if:
/// - either line number is 0 (line numbers are 1-based)
/// - `start_line > end_line` (checked after resolving [`EOF_SENTINEL`])
/// - either line number exceeds the number of lines in the view (after resolving)
///
/// [`EOF_SENTINEL`] in either position resolves to the total line count of the
/// file (the last line number).
pub fn line_range_to_source_bytes(
    view: &View,
    start_line: usize,
    end_line: usize,
) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    if start_line == 0 {
        return Err("edit: line numbers are 1-based (minimum 1)".into());
    }
    if start_line > end_line {
        return Err(format!("edit: start line {start_line} is after end line {end_line}").into());
    }

    // Build the normalised start-of-line position table.
    // line_starts[i] is the normalised byte offset of the start of line i+1.
    let mut line_starts: Vec<usize> = vec![0];
    for (i, &b) in view.bytes.iter().enumerate() {
        if b == b'\n' {
            line_starts.push(i + 1);
        }
    }

    // Total number of content lines.
    let total_lines = if view.bytes.is_empty() {
        0
    } else if view.bytes.last() == Some(&b'\n') {
        // The final push added a past-EOF position; discount it.
        line_starts.len() - 1
    } else {
        line_starts.len()
    };

    // Resolve EOF_SENTINEL to the total line count (last line).
    let start_line = if start_line == EOF_SENTINEL {
        total_lines
    } else {
        start_line
    };
    let end_line = if end_line == EOF_SENTINEL {
        total_lines
    } else {
        end_line
    };

    if start_line > total_lines {
        return Err(format!(
            "edit: line {start_line} is out of range (file has {total_lines} line{})",
            if total_lines == 1 { "" } else { "s" }
        )
        .into());
    }
    if end_line > total_lines {
        return Err(format!(
            "edit: line {end_line} is out of range (file has {total_lines} line{})",
            if total_lines == 1 { "" } else { "s" }
        )
        .into());
    }

    // Normalised byte range for [start_line, end_line] (inclusive).
    let norm_start = line_starts[start_line - 1];
    // line_starts[end_line] is the start of the next line (just past the
    // '\n' of end_line).  If end_line is the last line and has no trailing
    // '\n', there is no such entry and we use view.bytes.len() instead.
    let norm_end = if end_line < line_starts.len() {
        line_starts[end_line]
    } else {
        view.bytes.len()
    };

    // Translate normalised offsets to branch-absolute source byte offsets.
    let source_start = view.byte_range_start() + view.offset_map.to_source(norm_start as u64);
    let source_end = view.byte_range_start() + view.offset_map.to_source(norm_end as u64);

    Ok((source_start as usize, source_end as usize))
}

// ── CLI parsing helpers ───────────────────────────────────────────────────────

/// Parse a `--delete` / `--splice` RANGE string in binary mode.
///
/// Formats accepted:
/// - `N`   — `[N, N+1)` (single byte at 0-based offset N)
/// - `N-M` — `[N, M)` (0-based, exclusive end)
/// - `$` or `EOF` (case-insensitive) in either position — [`EOF_SENTINEL`];
///   resolves to the file's byte length in [`run`].
///
/// N and M may be decimal integers or `0x`/`0X`-prefixed hex.  Returns
/// `(start, end)` where `end` is exclusive.
pub fn parse_byte_range(s: &str) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    if let Some((lo_s, hi_s)) = s.split_once('-') {
        let lo = parse_byte_pos(lo_s).map_err(|e| format!("invalid range start in {s:?}: {e}"))?;
        let hi = parse_byte_pos(hi_s).map_err(|e| format!("invalid range end in {s:?}: {e}"))?;
        Ok((lo, hi))
    } else {
        let n = parse_byte_pos(s).map_err(|e| format!("invalid offset in {s:?}: {e}"))?;
        // Single EOF_SENTINEL is a zero-length range at the end of the file
        // (most useful as an implicit append target for --splice).
        if n == EOF_SENTINEL {
            Ok((EOF_SENTINEL, EOF_SENTINEL))
        } else {
            Ok((n, n + 1))
        }
    }
}

/// Parse a 0-based byte position or the end-of-file sentinel.
///
/// Accepts decimal integers, `0x`/`0X`-prefixed hex, or `$`/`EOF`
/// (case-insensitive), which returns [`EOF_SENTINEL`].
pub fn parse_byte_pos(s: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let t = s.trim();
    if t == "$" || t.eq_ignore_ascii_case("eof") {
        return Ok(EOF_SENTINEL);
    }
    parse_usize_maybe_hex(t)
}

/// Parse a `--delete` / `--splice` RANGE string in line mode.
///
/// Formats accepted:
/// - `N`   — single line N (1-based inclusive).
/// - `N-M` — lines N through M (1-based inclusive).
/// - `$` or `EOF` (case-insensitive) in either position — [`EOF_SENTINEL`];
///   resolves to the total line count in [`line_range_to_source_bytes`].
///
/// Returns `(start_line, end_line)` both 1-based inclusive.
pub fn parse_line_range(s: &str) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    /// Parse one side of a line range, accepting `$`/`EOF` as EOF_SENTINEL.
    fn parse_line_pos(s: &str) -> Result<usize, Box<dyn std::error::Error>> {
        let t = s.trim();
        if t == "$" || t.eq_ignore_ascii_case("eof") {
            return Ok(EOF_SENTINEL);
        }
        let n: usize = t
            .parse()
            .map_err(|_| format!("invalid line number {s:?}"))?;
        if n == 0 {
            return Err("line numbers are 1-based (minimum 1)".into());
        }
        Ok(n)
    }

    if let Some((lo_s, hi_s)) = s.split_once('-') {
        let lo = parse_line_pos(lo_s).map_err(|e| format!("invalid range start in {s:?}: {e}"))?;
        let hi = parse_line_pos(hi_s).map_err(|e| format!("invalid range end in {s:?}: {e}"))?;
        // Defer ordering check to line_range_to_source_bytes so that
        // EOF_SENTINEL is resolved before comparing.
        if lo != EOF_SENTINEL && hi != EOF_SENTINEL && lo > hi {
            return Err(format!("line range start {lo} is after end {hi}").into());
        }
        Ok((lo, hi))
    } else {
        let t = s.trim();
        if t == "$" || t.eq_ignore_ascii_case("eof") {
            return Ok((EOF_SENTINEL, EOF_SENTINEL));
        }
        let n: usize = t
            .parse()
            .map_err(|_| format!("invalid line number {s:?}"))?;
        if n == 0 {
            return Err("line numbers are 1-based (minimum 1)".into());
        }
        Ok((n, n))
    }
}

/// Parse a 1-based line number for `--insert` in line mode.
///
/// Accepts a positive decimal integer, or `$`/`EOF` (case-insensitive) which
/// returns [`EOF_SENTINEL`] (resolved to "append after last line" in `run_line`).
pub fn parse_line_num(s: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let t = s.trim();
    if t == "$" || t.eq_ignore_ascii_case("eof") {
        return Ok(EOF_SENTINEL);
    }
    let n: usize = t
        .parse()
        .map_err(|_| format!("invalid line number {s:?}"))?;
    if n == 0 {
        return Err("line numbers are 1-based (minimum 1)".into());
    }
    Ok(n)
}

/// Parse a decimal or `0x`/`0X`-prefixed hex integer into a `usize`.
pub fn parse_usize_maybe_hex(s: &str) -> Result<usize, Box<dyn std::error::Error>> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        usize::from_str_radix(hex, 16).map_err(|_| format!("invalid hex integer {s:?}").into())
    } else {
        s.parse::<usize>()
            .map_err(|_| format!("invalid integer {s:?}").into())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use harrier::{encoding::SourceConfig, source::Source};
    use tempfile::TempDir;

    use super::*;

    // ── parse_byte_range ──────────────────────────────────────────────────────

    #[test]
    fn range_single_decimal() {
        assert_eq!(parse_byte_range("5").unwrap(), (5, 6));
    }

    #[test]
    fn range_single_zero() {
        assert_eq!(parse_byte_range("0").unwrap(), (0, 1));
    }

    #[test]
    fn range_pair_decimal() {
        assert_eq!(parse_byte_range("3-7").unwrap(), (3, 7));
    }

    #[test]
    fn range_pair_adjacent() {
        // Zero-length range (start == end) is syntactically valid.
        assert_eq!(parse_byte_range("5-5").unwrap(), (5, 5));
    }

    #[test]
    fn range_single_hex() {
        assert_eq!(parse_byte_range("0x5").unwrap(), (5, 6));
    }

    #[test]
    fn range_pair_hex() {
        assert_eq!(parse_byte_range("0x3-0xA").unwrap(), (3, 10));
    }

    #[test]
    fn range_mixed_hex_decimal() {
        assert_eq!(parse_byte_range("0x10-32").unwrap(), (16, 32));
    }

    #[test]
    fn range_uppercase_hex() {
        assert_eq!(parse_byte_range("0XFF").unwrap(), (255, 256));
    }

    #[test]
    fn range_large_decimal() {
        assert_eq!(parse_byte_range("100-200").unwrap(), (100, 200));
    }

    #[test]
    fn range_invalid_not_a_number() {
        assert!(parse_byte_range("abc").is_err());
    }

    #[test]
    fn range_invalid_bad_end() {
        assert!(parse_byte_range("5-xyz").is_err());
    }

    #[test]
    fn range_invalid_empty() {
        assert!(parse_byte_range("").is_err());
    }

    // ── parse_usize_maybe_hex ─────────────────────────────────────────────────

    #[test]
    fn hex_zero() {
        assert_eq!(parse_usize_maybe_hex("0x0").unwrap(), 0);
    }

    #[test]
    fn hex_ff() {
        assert_eq!(parse_usize_maybe_hex("0xFF").unwrap(), 255);
    }

    #[test]
    fn decimal_plain() {
        assert_eq!(parse_usize_maybe_hex("42").unwrap(), 42);
    }

    #[test]
    fn decimal_zero() {
        assert_eq!(parse_usize_maybe_hex("0").unwrap(), 0);
    }

    #[test]
    fn hex_invalid() {
        assert!(parse_usize_maybe_hex("0xGG").is_err());
    }

    #[test]
    fn decimal_invalid() {
        assert!(parse_usize_maybe_hex("not_a_number").is_err());
    }

    // ── run_binary (via run) ──────────────────────────────────────────────────

    /// Test-only wrapper that forwards to `run()` with `IoMode::Mmap` and a
    /// permissive write policy (so the M2 mojibake guard does not affect
    /// tests that pre-date it).
    fn run_test(
        file: &Path,
        ops: Vec<EditOp>,
        binary: bool,
        line_ending_override: Option<LineEnding>,
        diff_out: Option<&mut dyn Write>,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        run(
            file,
            ops,
            binary,
            line_ending_override,
            diff_out,
            IoMode::Mmap,
            crate::mojibake::WritePolicy::permissive(),
        )
    }

    fn write_tmp(dir: &TempDir, name: &str, content: &[u8]) -> std::path::PathBuf {
        let p = dir.path().join(name);
        fs::write(&p, content).unwrap();
        p
    }

    fn read_file_bytes(path: &std::path::Path) -> Vec<u8> {
        fs::read(path).unwrap()
    }

    // 1. Delete middle bytes.
    #[test]
    fn binary_delete_middle() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCDEFGHIJ");
        // Delete bytes [3, 6): "DEF"
        let n = run_test(
            &p,
            vec![EditOp::Delete { start: 3, end: 6 }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(read_file_bytes(&p), b"ABCGHIJ");
    }

    // 2. Delete first byte.
    #[test]
    fn binary_delete_first() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"XABCDE");
        run_test(
            &p,
            vec![EditOp::Delete { start: 0, end: 1 }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"ABCDE");
    }

    // 3. Delete last byte.
    #[test]
    fn binary_delete_last() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCDEX");
        run_test(
            &p,
            vec![EditOp::Delete { start: 5, end: 6 }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"ABCDE");
    }

    // 4. Delete all bytes.
    #[test]
    fn binary_delete_all() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCDE");
        run_test(
            &p,
            vec![EditOp::Delete { start: 0, end: 5 }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"");
    }

    // 5. Insert at beginning.
    #[test]
    fn binary_insert_at_start() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"BCDE");
        run_test(
            &p,
            vec![EditOp::Insert {
                offset: 0,
                data: b"A".to_vec(),
            }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"ABCDE");
    }

    // 6. Insert in middle.
    #[test]
    fn binary_insert_middle() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABDE");
        run_test(
            &p,
            vec![EditOp::Insert {
                offset: 2,
                data: b"C".to_vec(),
            }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"ABCDE");
    }

    // 7. Insert at end (append).
    #[test]
    fn binary_insert_at_end() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCD");
        run_test(
            &p,
            vec![EditOp::Insert {
                offset: 4,
                data: b"E".to_vec(),
            }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"ABCDE");
    }

    // 8. Splice: replace with longer content.
    #[test]
    fn binary_splice_grow() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"AXXXE");
        run_test(
            &p,
            vec![EditOp::Splice {
                start: 1,
                end: 4,
                data: b"BCD".to_vec(),
            }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"ABCDE");
    }

    // 9. Splice: replace with shorter content.
    #[test]
    fn binary_splice_shrink() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCCCDE");
        run_test(
            &p,
            vec![EditOp::Splice {
                start: 2,
                end: 5,
                data: b"C".to_vec(),
            }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"ABCDE");
    }

    // 10. Multi-op composability: three non-overlapping ops, positions reference original.
    #[test]
    fn binary_multi_op_composable() {
        let dir = TempDir::new().unwrap();
        // Original: "A_BCXXDE"
        let p = write_tmp(&dir, "f.bin", b"A_BCXXDE");
        let ops = vec![
            EditOp::Delete { start: 1, end: 2 }, // remove '_'  at [1,2)
            EditOp::Splice {
                start: 4,
                end: 6,
                data: b"".to_vec(),
            }, // remove "XX" at [4,6)
            EditOp::Insert {
                offset: 8,
                data: b"!".to_vec(),
            }, // append '!'  at end
        ];
        run_test(&p, ops, true, None, None).unwrap();
        assert_eq!(read_file_bytes(&p), b"ABCDE!");
    }

    // 11. Ops in forward vs reverse CLI order produce the same result.
    #[test]
    fn binary_order_independence() {
        let dir = TempDir::new().unwrap();
        let content = b"ABCDE";

        let p1 = write_tmp(&dir, "f1.bin", content);
        run_test(
            &p1,
            vec![
                EditOp::Delete { start: 0, end: 1 },
                EditOp::Delete { start: 3, end: 4 },
            ],
            true,
            None,
            None,
        )
        .unwrap();

        let p2 = write_tmp(&dir, "f2.bin", content);
        run_test(
            &p2,
            vec![
                EditOp::Delete { start: 3, end: 4 },
                EditOp::Delete { start: 0, end: 1 },
            ],
            true,
            None,
            None,
        )
        .unwrap();

        assert_eq!(read_file_bytes(&p1), read_file_bytes(&p2));
    }

    // 12. Zero ops returns 0 without touching the file.
    #[test]
    fn binary_empty_ops() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCDE");
        let n = run_test(&p, vec![], true, None, None).unwrap();
        assert_eq!(n, 0);
        assert_eq!(read_file_bytes(&p), b"ABCDE");
    }

    // 13. Out-of-range end offset → error before file modification.
    #[test]
    fn binary_out_of_range_end() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCDE");
        let result = run_test(
            &p,
            vec![EditOp::Delete { start: 3, end: 10 }],
            true,
            None,
            None,
        );
        assert!(result.is_err(), "expected error for out-of-range range");
        assert_eq!(read_file_bytes(&p), b"ABCDE");
    }

    // 14. Insert offset past EOF → error before file modification.
    #[test]
    fn binary_insert_past_eof() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCDE");
        let result = run_test(
            &p,
            vec![EditOp::Insert {
                offset: 99,
                data: b"X".to_vec(),
            }],
            true,
            None,
            None,
        );
        assert!(result.is_err());
        assert_eq!(read_file_bytes(&p), b"ABCDE");
    }

    // 15. Overlapping ranges → error before file modification.
    #[test]
    fn binary_overlapping_ranges() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCDE");
        let ops = vec![
            EditOp::Delete { start: 1, end: 4 },
            EditOp::Delete { start: 3, end: 5 }, // overlaps [1,4)
        ];
        let result = run_test(&p, ops, true, None, None);
        assert!(result.is_err(), "expected error for overlapping ranges");
        assert_eq!(read_file_bytes(&p), b"ABCDE");
    }

    // 16. Adjacent ranges are permitted (end of one == start of next).
    #[test]
    fn binary_adjacent_ranges_ok() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCDE");
        let ops = vec![
            EditOp::Delete { start: 0, end: 2 }, // [0,2) removes "AB"
            EditOp::Delete { start: 2, end: 4 }, // [2,4) removes "CD"
        ];
        run_test(&p, ops, true, None, None).unwrap();
        assert_eq!(read_file_bytes(&p), b"E");
    }

    // 17. Invalid range: start > end → error.
    #[test]
    fn binary_invalid_range_start_gt_end() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCDE");
        let result = run_test(
            &p,
            vec![EditOp::Delete { start: 4, end: 2 }],
            true,
            None,
            None,
        );
        assert!(result.is_err());
        assert_eq!(read_file_bytes(&p), b"ABCDE");
    }

    // 18. Single-byte file: delete the only byte.
    #[test]
    fn binary_single_byte_file() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"X");
        run_test(
            &p,
            vec![EditOp::Delete { start: 0, end: 1 }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"");
    }

    // 19. .bak file is created on successful write.
    #[test]
    fn binary_bak_created() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCDE");
        run_test(
            &p,
            vec![EditOp::Delete { start: 0, end: 1 }],
            true,
            None,
            None,
        )
        .unwrap();
        let bak = dir
            .path()
            .join(format!("{}.bak", p.file_name().unwrap().to_string_lossy()));
        assert!(bak.exists(), "expected a .bak file at {}", bak.display());
    }

    // 20. Empty ops in line mode returns 0, file unchanged.
    #[test]
    fn line_mode_empty_ops() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"hello\n");
        let n = run_test(&p, vec![], false, None, None).unwrap();
        assert_eq!(n, 0);
        assert_eq!(read_file_bytes(&p), b"hello\n");
    }

    // ── line_range_to_source_bytes ────────────────────────────────────────────

    // Build a View over the given raw file content for use in line-range tests.
    fn make_view(content: &[u8]) -> (View, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, content).unwrap();

        let branch = crate::open_as_branch(&path, IoMode::Mmap).unwrap();
        let file_len = branch.byte_len();
        let source = Source::new(Arc::clone(&branch), SourceConfig::default()).unwrap();
        let lines = source.as_lines().unwrap();
        let view = lines.view_range(0..file_len).unwrap();
        (view, dir)
    }

    // 21. First line of an LF file.
    #[test]
    fn lrsb_lf_first_line() {
        let (view, _dir) = make_view(b"abc\ndef\nghi\n");
        assert_eq!(line_range_to_source_bytes(&view, 1, 1).unwrap(), (0, 4));
    }

    // 22. Middle line of an LF file.
    #[test]
    fn lrsb_lf_middle_line() {
        let (view, _dir) = make_view(b"abc\ndef\nghi\n");
        assert_eq!(line_range_to_source_bytes(&view, 2, 2).unwrap(), (4, 8));
    }

    // 23. Last line of an LF file (trailing newline).
    #[test]
    fn lrsb_lf_last_line_with_nl() {
        let (view, _dir) = make_view(b"abc\ndef\nghi\n");
        assert_eq!(line_range_to_source_bytes(&view, 3, 3).unwrap(), (8, 12));
    }

    // 24. Last line of an LF file (no trailing newline).
    #[test]
    fn lrsb_lf_last_line_no_nl() {
        let (view, _dir) = make_view(b"abc\ndef\nghi");
        assert_eq!(line_range_to_source_bytes(&view, 3, 3).unwrap(), (8, 11));
    }

    // 25. Multi-line range spanning first two lines.
    #[test]
    fn lrsb_lf_multi_first_two() {
        let (view, _dir) = make_view(b"abc\ndef\nghi\n");
        assert_eq!(line_range_to_source_bytes(&view, 1, 2).unwrap(), (0, 8));
    }

    // 26. Multi-line range spanning last two lines.
    #[test]
    fn lrsb_lf_multi_last_two() {
        let (view, _dir) = make_view(b"abc\ndef\nghi\n");
        assert_eq!(line_range_to_source_bytes(&view, 2, 3).unwrap(), (4, 12));
    }

    // 27. Multi-line range spanning entire file.
    #[test]
    fn lrsb_lf_entire_file() {
        let (view, _dir) = make_view(b"abc\ndef\nghi\n");
        assert_eq!(line_range_to_source_bytes(&view, 1, 3).unwrap(), (0, 12));
    }

    // 28. Single-line file with trailing newline.
    #[test]
    fn lrsb_lf_single_line_file_with_nl() {
        let (view, _dir) = make_view(b"hello\n");
        assert_eq!(line_range_to_source_bytes(&view, 1, 1).unwrap(), (0, 6));
    }

    // 29. Single-line file without trailing newline.
    #[test]
    fn lrsb_lf_single_line_file_no_nl() {
        let (view, _dir) = make_view(b"hello");
        assert_eq!(line_range_to_source_bytes(&view, 1, 1).unwrap(), (0, 5));
    }

    // 30. Empty line in file (blank line between two content lines).
    #[test]
    fn lrsb_lf_empty_line_in_middle() {
        // "abc\n\ndef\n" — line 2 is an empty line (just a newline)
        let (view, _dir) = make_view(b"abc\n\ndef\n");
        assert_eq!(line_range_to_source_bytes(&view, 2, 2).unwrap(), (4, 5));
    }

    // 31. CRLF file — first line.
    #[test]
    fn lrsb_crlf_first_line() {
        let (view, _dir) = make_view(b"abc\r\ndef\r\nghi\r\n");
        // Line 1 = "abc\r\n" at source [0, 5)
        assert_eq!(line_range_to_source_bytes(&view, 1, 1).unwrap(), (0, 5));
    }

    // 32. CRLF file — middle line.
    #[test]
    fn lrsb_crlf_middle_line() {
        let (view, _dir) = make_view(b"abc\r\ndef\r\nghi\r\n");
        // Line 2 = "def\r\n" at source [5, 10)
        assert_eq!(line_range_to_source_bytes(&view, 2, 2).unwrap(), (5, 10));
    }

    // 33. CRLF file — last line.
    #[test]
    fn lrsb_crlf_last_line() {
        let (view, _dir) = make_view(b"abc\r\ndef\r\nghi\r\n");
        // Line 3 = "ghi\r\n" at source [10, 15)
        assert_eq!(line_range_to_source_bytes(&view, 3, 3).unwrap(), (10, 15));
    }

    // 34. CRLF file — multi-line range.
    #[test]
    fn lrsb_crlf_multi_line() {
        let (view, _dir) = make_view(b"abc\r\ndef\r\nghi\r\n");
        // Lines 1-2 = "abc\r\ndef\r\n" at source [0, 10)
        assert_eq!(line_range_to_source_bytes(&view, 1, 2).unwrap(), (0, 10));
    }

    // 35. CR file — first line.
    #[test]
    fn lrsb_cr_first_line() {
        let (view, _dir) = make_view(b"abc\rdef\rghi\r");
        // CR is 1:1 with LF so source offsets equal normalised offsets.
        // Line 1 = "abc\r" at source [0, 4)
        assert_eq!(line_range_to_source_bytes(&view, 1, 1).unwrap(), (0, 4));
    }

    // 36. CR file — multi-line range.
    #[test]
    fn lrsb_cr_multi_line() {
        let (view, _dir) = make_view(b"abc\rdef\rghi\r");
        // Lines 2-3 = "def\rghi\r" at source [4, 12)
        assert_eq!(line_range_to_source_bytes(&view, 2, 3).unwrap(), (4, 12));
    }

    // 37. Out-of-range start line → error.
    #[test]
    fn lrsb_out_of_range_start() {
        let (view, _dir) = make_view(b"abc\ndef\n");
        assert!(line_range_to_source_bytes(&view, 3, 3).is_err());
    }

    // 38. Out-of-range end line → error.
    #[test]
    fn lrsb_out_of_range_end() {
        let (view, _dir) = make_view(b"abc\ndef\n");
        assert!(line_range_to_source_bytes(&view, 1, 5).is_err());
    }

    // 39. start > end → error.
    #[test]
    fn lrsb_start_after_end() {
        let (view, _dir) = make_view(b"abc\ndef\n");
        assert!(line_range_to_source_bytes(&view, 2, 1).is_err());
    }

    // 40. Line number 0 → error (1-based).
    #[test]
    fn lrsb_zero_line_number() {
        let (view, _dir) = make_view(b"abc\ndef\n");
        assert!(line_range_to_source_bytes(&view, 0, 1).is_err());
    }

    // 41. Empty file → error.
    #[test]
    fn lrsb_empty_file() {
        let (view, _dir) = make_view(b"");
        assert!(line_range_to_source_bytes(&view, 1, 1).is_err());
    }

    // ── run_line (via run) ────────────────────────────────────────────────────

    // 42. LF file — delete first line.
    #[test]
    fn lm_lf_delete_first_line() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\ndef\nghi\n");
        let n = run_test(
            &p,
            vec![EditOp::Delete { start: 1, end: 1 }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(read_file_bytes(&p), b"def\nghi\n");
    }

    // 43. LF file — delete last line (trailing newline case).
    #[test]
    fn lm_lf_delete_last_line() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\ndef\nghi\n");
        run_test(
            &p,
            vec![EditOp::Delete { start: 3, end: 3 }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"abc\ndef\n");
    }

    // 44. LF file — delete middle line.
    #[test]
    fn lm_lf_delete_middle_line() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\ndef\nghi\n");
        run_test(
            &p,
            vec![EditOp::Delete { start: 2, end: 2 }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"abc\nghi\n");
    }

    // 45. LF file — delete multi-line range.
    #[test]
    fn lm_lf_delete_multi_line() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\ndef\nghi\njkl\n");
        run_test(
            &p,
            vec![EditOp::Delete { start: 2, end: 3 }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"abc\njkl\n");
    }

    // 46. LF file — insert before first line.
    #[test]
    fn lm_lf_insert_before_first() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\ndef\n");
        run_test(
            &p,
            vec![EditOp::Insert {
                offset: 1,
                data: b"ZZZ\n".to_vec(),
            }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"ZZZ\nabc\ndef\n");
    }

    // 47. LF file — insert before middle line.
    #[test]
    fn lm_lf_insert_before_middle() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\nghi\n");
        run_test(
            &p,
            vec![EditOp::Insert {
                offset: 2,
                data: b"def\n".to_vec(),
            }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"abc\ndef\nghi\n");
    }

    // 48. LF file — splice (replace) single line with shorter content.
    #[test]
    fn lm_lf_splice_shrink() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\nXXXXX\nghi\n");
        run_test(
            &p,
            vec![EditOp::Splice {
                start: 2,
                end: 2,
                data: b"def\n".to_vec(),
            }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"abc\ndef\nghi\n");
    }

    // 49. LF file — splice multi-line range with single replacement.
    #[test]
    fn lm_lf_splice_multi_to_one() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\nXXX\nYYY\nghi\n");
        run_test(
            &p,
            vec![EditOp::Splice {
                start: 2,
                end: 3,
                data: b"def\n".to_vec(),
            }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"abc\ndef\nghi\n");
    }

    // 50. CRLF file — delete a line, verify CRLF endings preserved.
    #[test]
    fn lm_crlf_delete_middle_line() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\r\ndef\r\nghi\r\n");
        run_test(
            &p,
            vec![EditOp::Delete { start: 2, end: 2 }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"abc\r\nghi\r\n");
    }

    // 51. CRLF file — insert a line; data \n is denormalised to \r\n.
    #[test]
    fn lm_crlf_insert_line() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\r\nghi\r\n");
        run_test(
            &p,
            vec![EditOp::Insert {
                offset: 2,
                data: b"def\n".to_vec(),
            }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"abc\r\ndef\r\nghi\r\n");
    }

    // 52. CRLF file — splice a line; data \n denormalised to \r\n.
    #[test]
    fn lm_crlf_splice_line() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\r\nXXX\r\nghi\r\n");
        run_test(
            &p,
            vec![EditOp::Splice {
                start: 2,
                end: 2,
                data: b"def\n".to_vec(),
            }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"abc\r\ndef\r\nghi\r\n");
    }

    // 53. CR file — delete a line.
    #[test]
    fn lm_cr_delete_line() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\rdef\rghi\r");
        run_test(
            &p,
            vec![EditOp::Delete { start: 2, end: 2 }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"abc\rghi\r");
    }

    // 54. Multi-op composability: delete lines 1 and 3 in original coords.
    #[test]
    fn lm_multi_op_composable() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"aa\nbb\ncc\ndd\n");
        let ops = vec![
            EditOp::Delete { start: 1, end: 1 }, // remove "aa\n"
            EditOp::Delete { start: 3, end: 3 }, // remove "cc\n" (original coord)
        ];
        run_test(&p, ops, false, None, None).unwrap();
        assert_eq!(read_file_bytes(&p), b"bb\ndd\n");
    }

    // 55. Order independence: same deletes in reverse CLI order.
    #[test]
    fn lm_order_independence() {
        let content = b"aa\nbb\ncc\ndd\n";
        let dir = TempDir::new().unwrap();

        let p1 = write_tmp(&dir, "f1.txt", content);
        run_test(
            &p1,
            vec![
                EditOp::Delete { start: 1, end: 1 },
                EditOp::Delete { start: 3, end: 3 },
            ],
            false,
            None,
            None,
        )
        .unwrap();

        let p2 = write_tmp(&dir, "f2.txt", content);
        run_test(
            &p2,
            vec![
                EditOp::Delete { start: 3, end: 3 },
                EditOp::Delete { start: 1, end: 1 },
            ],
            false,
            None,
            None,
        )
        .unwrap();

        assert_eq!(read_file_bytes(&p1), read_file_bytes(&p2));
    }

    // 56. Overlapping line ranges → error, file unchanged.
    #[test]
    fn lm_overlapping_ranges() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"aa\nbb\ncc\ndd\n");
        let ops = vec![
            EditOp::Delete { start: 2, end: 3 },
            EditOp::Delete { start: 3, end: 4 }, // overlaps at line 3
        ];
        assert!(run_test(&p, ops, false, None, None).is_err());
        assert_eq!(read_file_bytes(&p), b"aa\nbb\ncc\ndd\n");
    }

    // 57. Out-of-range line → error, file unchanged.
    #[test]
    fn lm_out_of_range() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\ndef\n");
        assert!(
            run_test(
                &p,
                vec![EditOp::Delete { start: 5, end: 5 }],
                false,
                None,
                None
            )
            .is_err()
        );
        assert_eq!(read_file_bytes(&p), b"abc\ndef\n");
    }

    // 58. .bak file created in line mode.
    #[test]
    fn lm_bak_created() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\ndef\n");
        run_test(
            &p,
            vec![EditOp::Delete { start: 1, end: 1 }],
            false,
            None,
            None,
        )
        .unwrap();
        let bak = dir
            .path()
            .join(format!("{}.bak", p.file_name().unwrap().to_string_lossy()));
        assert!(bak.exists(), "expected .bak at {}", bak.display());
    }

    // 59. Line-ending override: splice in LF file, force CRLF on replacement.
    #[test]
    fn lm_line_ending_override() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\ndef\nghi\n");
        run_test(
            &p,
            vec![EditOp::Splice {
                start: 2,
                end: 2,
                data: b"XXX\n".to_vec(),
            }],
            false,
            Some(LineEnding::CrLf),
            None,
        )
        .unwrap();
        // The replacement "XXX\n" must be written as "XXX\r\n".
        let got = read_file_bytes(&p);
        assert!(
            got.contains(&b'\r'),
            "expected \\r in replaced line after override"
        );
        assert!(
            got.windows(2).any(|w| w == b"\r\n"),
            "expected \\r\\n sequence"
        );
    }

    // 60. No-trailing-newline LF file — delete last line.
    #[test]
    fn lm_lf_delete_last_line_no_nl() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\ndef");
        run_test(
            &p,
            vec![EditOp::Delete { start: 2, end: 2 }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"abc\n");
    }

    // 61. parse_line_range: single decimal.
    #[test]
    fn plr_single() {
        assert_eq!(parse_line_range("5").unwrap(), (5, 5));
    }

    // 62. parse_line_range: pair.
    #[test]
    fn plr_pair() {
        assert_eq!(parse_line_range("3-7").unwrap(), (3, 7));
    }

    // 63. parse_line_range: zero → error.
    #[test]
    fn plr_zero() {
        assert!(parse_line_range("0").is_err());
    }

    // 64. parse_line_range: start > end → error.
    #[test]
    fn plr_start_after_end() {
        assert!(parse_line_range("5-3").is_err());
    }

    // 65. parse_line_num: valid.
    #[test]
    fn pln_valid() {
        assert_eq!(parse_line_num("10").unwrap(), 10);
    }

    // 66. parse_line_num: zero → error.
    #[test]
    fn pln_zero() {
        assert!(parse_line_num("0").is_err());
    }

    // ── EOF sentinel (ED-10) ──────────────────────────────────────────────────

    // 67. parse_line_num: "$" → EOF_SENTINEL.
    #[test]
    fn pln_dollar_sentinel() {
        assert_eq!(parse_line_num("$").unwrap(), EOF_SENTINEL);
    }

    // 68. parse_line_num: "EOF" (case-insensitive) → EOF_SENTINEL.
    #[test]
    fn pln_eof_ci() {
        assert_eq!(parse_line_num("EOF").unwrap(), EOF_SENTINEL);
        assert_eq!(parse_line_num("eof").unwrap(), EOF_SENTINEL);
        assert_eq!(parse_line_num("Eof").unwrap(), EOF_SENTINEL);
    }

    // 69. parse_line_range: "$" → (EOF_SENTINEL, EOF_SENTINEL).
    #[test]
    fn plr_dollar_single() {
        assert_eq!(parse_line_range("$").unwrap(), (EOF_SENTINEL, EOF_SENTINEL));
    }

    // 70. parse_line_range: "3-$" → (3, EOF_SENTINEL).
    #[test]
    fn plr_n_to_dollar() {
        assert_eq!(parse_line_range("3-$").unwrap(), (3, EOF_SENTINEL));
    }

    // 71. parse_line_range: "EOF" single → (EOF_SENTINEL, EOF_SENTINEL).
    #[test]
    fn plr_eof_single() {
        assert_eq!(
            parse_line_range("EOF").unwrap(),
            (EOF_SENTINEL, EOF_SENTINEL)
        );
    }

    // 72. parse_line_range: "2-eof" → (2, EOF_SENTINEL).
    #[test]
    fn plr_n_to_eof() {
        assert_eq!(parse_line_range("2-eof").unwrap(), (2, EOF_SENTINEL));
    }

    // 73. parse_byte_pos: "$" → EOF_SENTINEL.
    #[test]
    fn pbp_dollar() {
        assert_eq!(parse_byte_pos("$").unwrap(), EOF_SENTINEL);
    }

    // 74. parse_byte_pos: "EOF" → EOF_SENTINEL.
    #[test]
    fn pbp_eof() {
        assert_eq!(parse_byte_pos("EOF").unwrap(), EOF_SENTINEL);
        assert_eq!(parse_byte_pos("eof").unwrap(), EOF_SENTINEL);
    }

    // 75. parse_byte_pos: hex number still works.
    #[test]
    fn pbp_hex() {
        assert_eq!(parse_byte_pos("0xff").unwrap(), 255);
    }

    // 76. parse_byte_range: "$" → (EOF_SENTINEL, EOF_SENTINEL) — no overflow.
    #[test]
    fn pbr_dollar_single() {
        assert_eq!(parse_byte_range("$").unwrap(), (EOF_SENTINEL, EOF_SENTINEL));
    }

    // 77. parse_byte_range: "5-$" → (5, EOF_SENTINEL).
    #[test]
    fn pbr_n_to_dollar() {
        assert_eq!(parse_byte_range("5-$").unwrap(), (5, EOF_SENTINEL));
    }

    // 78. parse_byte_range: "eof" single → (EOF_SENTINEL, EOF_SENTINEL).
    #[test]
    fn pbr_eof_single() {
        assert_eq!(
            parse_byte_range("eof").unwrap(),
            (EOF_SENTINEL, EOF_SENTINEL)
        );
    }

    // 79. line_range_to_source_bytes: EOF_SENTINEL as end → last line's end.
    #[test]
    fn lrtsb_eof_end() {
        let (view, _dir) = make_view(b"line1\nline2\nline3\n");
        // EOF_SENTINEL end resolves to total_lines=3; range 1-3.
        let (start, end) = line_range_to_source_bytes(&view, 1, EOF_SENTINEL).unwrap();
        let content = b"line1\nline2\nline3\n";
        assert_eq!(&content[start..end], b"line1\nline2\nline3\n");
    }

    // 80. line_range_to_source_bytes: EOF_SENTINEL as both → last line only.
    #[test]
    fn lrtsb_eof_both() {
        let (view, _dir) = make_view(b"line1\nline2\nline3\n");
        let (start, end) = line_range_to_source_bytes(&view, EOF_SENTINEL, EOF_SENTINEL).unwrap();
        let content = b"line1\nline2\nline3\n";
        assert_eq!(&content[start..end], b"line3\n");
    }

    // 81. run_test() line Delete $: deletes the last line.
    #[test]
    fn ed10_line_delete_dollar() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"aaa\nbbb\nccc\n");
        run_test(
            &p,
            vec![EditOp::Delete {
                start: EOF_SENTINEL,
                end: EOF_SENTINEL,
            }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"aaa\nbbb\n");
    }

    // 82. run_test() line Delete N-$: deletes from line N to end.
    #[test]
    fn ed10_line_delete_n_to_dollar() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"aaa\nbbb\nccc\nddd\n");
        run_test(
            &p,
            vec![EditOp::Delete {
                start: 3,
                end: EOF_SENTINEL,
            }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"aaa\nbbb\n");
    }

    // 83. run_test() line Insert $: appends a line at end.
    #[test]
    fn ed10_line_insert_dollar() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"aaa\nbbb\n");
        run_test(
            &p,
            vec![EditOp::Insert {
                offset: EOF_SENTINEL,
                data: b"ccc\n".to_vec(),
            }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"aaa\nbbb\nccc\n");
    }

    // 84. run_test() binary Delete N-$: deletes from byte N to end.
    #[test]
    fn ed10_binary_delete_n_to_dollar() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"ABCDEFGH");
        run_test(
            &p,
            vec![EditOp::Delete {
                start: 3,
                end: EOF_SENTINEL,
            }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"ABC");
    }

    // 85. run_test() binary Insert $: appends bytes at end.
    #[test]
    fn ed10_binary_insert_dollar() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"HELLO");
        run_test(
            &p,
            vec![EditOp::Insert {
                offset: EOF_SENTINEL,
                data: b" WORLD".to_vec(),
            }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"HELLO WORLD");
    }

    // ── validate integration (ED-4) ───────────────────────────────────────────

    // 67. Text-mode validate passes → edit proceeds normally.
    #[test]
    fn ed4_text_validate_passes() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\ndef\n");
        crate::cmd::validate::run_all(&["line:1".into(), "abc".into()], &p, false, IoMode::Mmap)
            .unwrap();
        run_test(
            &p,
            vec![EditOp::Delete { start: 2, end: 2 }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"abc\n");
    }

    // 68. Text-mode validate fails → run_test() never called; file and .bak unchanged.
    #[test]
    fn ed4_text_validate_fails_file_unchanged() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"abc\ndef\n");
        let before = read_file_bytes(&p);
        let err = crate::cmd::validate::run_all(
            &["line:1".into(), "xyz".into()],
            &p,
            false,
            IoMode::Mmap,
        );
        assert!(err.is_err(), "validate should fail");
        assert_eq!(read_file_bytes(&p), before);
        let bak = dir.path().join("f.txt.bak");
        assert!(
            !bak.exists(),
            "no .bak expected when validate fails before run"
        );
    }

    // 69. Binary-mode validate passes → edit proceeds normally.
    #[test]
    fn ed4_binary_validate_passes() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"\xDE\xAD\xBE\xEF");
        crate::cmd::validate::run_all(
            &["bytes:0-4".into(), "deadbeef".into()],
            &p,
            true,
            IoMode::Mmap,
        )
        .unwrap();
        run_test(
            &p,
            vec![EditOp::Splice {
                start: 0,
                end: 1,
                data: b"\xFF".to_vec(),
            }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"\xFF\xAD\xBE\xEF");
    }

    // 70. Binary-mode validate fails → run_test() never called; file and .bak unchanged.
    #[test]
    fn ed4_binary_validate_fails_file_unchanged() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"\xDE\xAD\xBE\xEF");
        let before = read_file_bytes(&p);
        // "00000000" ≠ "deadbeef".
        let err = crate::cmd::validate::run_all(
            &["bytes:0-4".into(), "00000000".into()],
            &p,
            true,
            IoMode::Mmap,
        );
        assert!(err.is_err(), "validate should fail");
        assert_eq!(read_file_bytes(&p), before);
        let bak = dir.path().join("f.bin.bak");
        assert!(
            !bak.exists(),
            "no .bak expected when validate fails before run"
        );
    }

    // ── diff output (ED-5) ───────────────────────────────────────────────────

    // 71. diff_out receives a unified diff after a line-mode delete.
    #[test]
    fn ed5_diff_on_delete() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"aaa\nbbb\nccc\n");
        let mut diff_buf: Vec<u8> = Vec::new();
        run_test(
            &p,
            vec![EditOp::Delete { start: 2, end: 2 }],
            false,
            None,
            Some(&mut diff_buf),
        )
        .unwrap();
        let diff_str = String::from_utf8(diff_buf).unwrap();
        // Diff must contain the removed line marked with '-'.
        assert!(
            diff_str.contains("-bbb"),
            "expected '-bbb' in diff: {diff_str:?}"
        );
        // The retained lines must NOT appear as removed.
        assert!(!diff_str.contains("-aaa"), "unexpected '-aaa' in diff");
        assert!(!diff_str.contains("-ccc"), "unexpected '-ccc' in diff");
    }

    // 72. diff_out receives a unified diff after a line-mode splice.
    #[test]
    fn ed5_diff_on_splice() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"aaa\nbbb\nccc\n");
        let mut diff_buf: Vec<u8> = Vec::new();
        run_test(
            &p,
            vec![EditOp::Splice {
                start: 2,
                end: 2,
                data: b"NEW\n".to_vec(),
            }],
            false,
            None,
            Some(&mut diff_buf),
        )
        .unwrap();
        let diff_str = String::from_utf8(diff_buf).unwrap();
        assert!(
            diff_str.contains("-bbb"),
            "expected '-bbb' in diff: {diff_str:?}"
        );
        assert!(
            diff_str.contains("+NEW"),
            "expected '+NEW' in diff: {diff_str:?}"
        );
    }

    // 73. diff_out receives a unified diff after a line-mode insert.
    #[test]
    fn ed5_diff_on_insert() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"aaa\nbbb\n");
        let mut diff_buf: Vec<u8> = Vec::new();
        run_test(
            &p,
            vec![EditOp::Insert {
                offset: 2,
                data: b"NEW\n".to_vec(),
            }],
            false,
            None,
            Some(&mut diff_buf),
        )
        .unwrap();
        let diff_str = String::from_utf8(diff_buf).unwrap();
        assert!(
            diff_str.contains("+NEW"),
            "expected '+NEW' in diff: {diff_str:?}"
        );
        // Existing lines must not appear as removed.
        assert!(!diff_str.contains("-aaa"), "unexpected '-aaa' in diff");
        assert!(!diff_str.contains("-bbb"), "unexpected '-bbb' in diff");
    }

    // 74. diff_out is empty when diff_out is None (no-op allocation path).
    #[test]
    fn ed5_no_diff_when_none() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"aaa\nbbb\n");
        // Pass None — must not panic and file must still be modified.
        run_test(
            &p,
            vec![EditOp::Delete { start: 1, end: 1 }],
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"bbb\n");
    }

    // 75. Binary mode: diff_out is ignored even when Some (no diff emitted).
    #[test]
    fn ed5_binary_diff_skipped() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"\x01\x02\x03\x04");
        let mut diff_buf: Vec<u8> = Vec::new();
        run_test(
            &p,
            vec![EditOp::Delete { start: 1, end: 2 }],
            true,
            None,
            Some(&mut diff_buf),
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"\x01\x03\x04");
        // diff_buf stays empty: binary mode never emits a diff.
        assert!(
            diff_buf.is_empty(),
            "expected no diff output in binary mode"
        );
    }

    // 76. CRLF file: diff is LF-normalised (no \\r in diff output).
    #[test]
    fn ed5_crlf_diff_normalised() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.txt", b"aaa\r\nbbb\r\nccc\r\n");
        let mut diff_buf: Vec<u8> = Vec::new();
        run_test(
            &p,
            vec![EditOp::Delete { start: 2, end: 2 }],
            false,
            None,
            Some(&mut diff_buf),
        )
        .unwrap();
        let diff_str = String::from_utf8(diff_buf).unwrap();
        // The diff content lines must not contain bare \r.
        assert!(
            !diff_str.contains('\r'),
            "expected no \\r in diff: {diff_str:?}"
        );
        assert!(diff_str.contains("-bbb"), "expected '-bbb' in diff");
    }

    // ── data-format decode for binary mode (ED-6) ─────────────────────────────
    //
    // data_format::decode is called in main.rs before constructing EditOps.
    // These tests simulate that path: decode the format string, then call run_test()
    // in binary mode with the resulting bytes.

    // 77. Hex data in --splice binary mode.
    #[test]
    fn ed6_hex_splice() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"\xAA\xBB\xCC\xDD");
        let decoded =
            crate::data_format::decode(&crate::data_format::DataFormat::Hex, "CAFE").unwrap();
        run_test(
            &p,
            vec![EditOp::Splice {
                start: 1,
                end: 3,
                data: decoded,
            }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"\xAA\xCA\xFE\xDD");
    }

    // 78. Hex data with dash separators in --insert binary mode.
    #[test]
    fn ed6_hex_with_dashes_insert() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"\x01\x04");
        let decoded =
            crate::data_format::decode(&crate::data_format::DataFormat::Hex, "02-03").unwrap();
        run_test(
            &p,
            vec![EditOp::Insert {
                offset: 1,
                data: decoded,
            }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"\x01\x02\x03\x04");
    }

    // 79. Base64 data in --splice binary mode.
    #[test]
    fn ed6_base64_splice() {
        let dir = TempDir::new().unwrap();
        // "AQID" is base64 for [0x01, 0x02, 0x03].
        let p = write_tmp(&dir, "f.bin", b"\xFF\xFF\xFF\xFF");
        let decoded =
            crate::data_format::decode(&crate::data_format::DataFormat::Base64, "AQID").unwrap();
        run_test(
            &p,
            vec![EditOp::Splice {
                start: 0,
                end: 3,
                data: decoded,
            }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"\x01\x02\x03\xFF");
    }

    // 80. Encoded (tpu escape codec) data in --splice binary mode.
    #[test]
    fn ed6_encoded_splice() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"hello world");
        // "\x41\x42\x43" = "ABC" in tpu escape codec.
        let decoded =
            crate::data_format::decode(&crate::data_format::DataFormat::Encoded, r"\x41\x42\x43")
                .unwrap();
        run_test(
            &p,
            vec![EditOp::Splice {
                start: 6,
                end: 11,
                data: decoded,
            }],
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(read_file_bytes(&p), b"hello ABC");
    }

    // 81. Invalid hex → decode error; run_test() never called; file unchanged.
    #[test]
    fn ed6_invalid_hex_error() {
        let dir = TempDir::new().unwrap();
        let p = write_tmp(&dir, "f.bin", b"\x01\x02\x03");
        let before = read_file_bytes(&p);
        // Odd number of hex digits.
        let err = crate::data_format::decode(&crate::data_format::DataFormat::Hex, "ABC");
        assert!(err.is_err(), "expected decode error for odd-length hex");
        // File must be untouched (run_test() was never called).
        assert_eq!(read_file_bytes(&p), before);
    }

    // 82. Invalid base64 → decode error; file unchanged.
    #[test]
    fn ed6_invalid_base64_error() {
        let err = crate::data_format::decode(&crate::data_format::DataFormat::Base64, "!!!");
        assert!(err.is_err(), "expected decode error for invalid base64");
    }
}
