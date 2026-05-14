// Copyright (c) 2026, Michael Grier

//! `tpu read` — emit a file as UTF-8/LF to a writer, with optional line
//! range selection and line-number prefixes.

use std::{fs, io::Write, path::Path, sync::Arc};

use harrier::{encoding::SourceConfig, source::Source};
use md5::{Digest, Md5};

use crate::{
    IoMode,
    encoding::{BomPolicy, OutputEncoding},
};

/// UTF-8 BOM byte sequence (U+FEFF encoded as UTF-8).
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Run the `read` subcommand.
///
/// Opens `file` with harrier to detect its encoding and line-ending convention,
/// materialises a normalised (LF-only) view of the requested byte range,
/// decodes to UTF-8, and writes the result to `out`.
///
/// `lines_range` is a 1-based inclusive `(start, end)` pair.  `None` emits
/// the entire file.  `numbers` prepends each line with its 1-based position.
///
/// `output_encoding` and `bom_policy` control whether a UTF-8 BOM is
/// prepended to the output.  They are only meaningful together:
/// `OutputEncoding::Utf8` enables BOM control; `OutputEncoding::Preserve`
/// never emits a BOM (the text content is always UTF-8 regardless).
///
/// `notes` is the optional advisory writer (Milestone 4).  When `Some`,
/// after decoding the requested content [`crate::mojibake::emit_read_advisory`]
/// is called against the *full* file's decoded text and may emit a
/// `note: <path>: …` line if mojibake is detected.  Pass `None` to
/// suppress the advisory entirely (e.g. `--no-mojibake-warning`).
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

    // Optionally prepend a UTF-8 BOM to stdout before any content.
    // This is only done when --utf8 is active; the text itself is always
    // decoded to UTF-8 regardless of the flag.
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

    // Decode from the original encoding (LF-normalised bytes) to a UTF-8
    // Cow<str>.  The view range already starts past the BOM bytes.
    let (text, _) = encoding.decode_without_bom_handling(&view.bytes);

    // Read-time advisory (Milestone 4): emit a one-line note to the
    // diagnostics writer if the decoded text appears to contain
    // mojibake.  Never blocks the read.
    if let Some(notes) = notes {
        crate::mojibake::emit_read_advisory(notes, file, &text)?;
    }

    // Split into lines.  A trailing '\n' in the source produces a trailing
    // empty string from split(); drop it so line numbers stay correct.
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

    for (i, line) in all_lines[start..end].iter().enumerate() {
        if numbers {
            writeln!(out, "{:>6}  {}", start + i + 1, line)?;
        } else {
            writeln!(out, "{}", line)?;
        }
    }

    Ok(())
}

/// Parse a `--lines` argument string into a 1-based inclusive range.
///
/// Accepts:
/// - `"N"` — single line N.
/// - `"N-M"` — lines N through M inclusive.
///
/// Returns `Err` with a human-readable message on invalid input.
pub fn parse_lines_arg(s: &str) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    if let Some((lo, hi)) = s.split_once('-') {
        let lo: usize = lo
            .trim()
            .parse()
            .map_err(|_| format!("invalid line number {lo:?} in --lines"))?;
        let hi: usize = hi
            .trim()
            .parse()
            .map_err(|_| format!("invalid line number {hi:?} in --lines"))?;
        if lo == 0 || hi == 0 {
            return Err("--lines: line numbers are 1-based (minimum 1)".into());
        }
        if lo > hi {
            return Err(format!("--lines: start {lo} is after end {hi}").into());
        }
        Ok((lo, hi))
    } else {
        let n: usize = s
            .trim()
            .parse()
            .map_err(|_| format!("invalid line number {s:?} in --lines"))?;
        if n == 0 {
            return Err("--lines: line numbers are 1-based (minimum 1)".into());
        }
        Ok((n, n))
    }
}

/// Run the `read --binary` subcommand.
///
/// Opens `file` as raw bytes (bypassing harrier encoding/line-ending detection),
/// escapes the selected byte range with [`crate::escape::encode_bytes`], and
/// writes the result to `out` with **no** trailing newline added.
///
/// `byte_range` is a 1-based inclusive `(start, end)` pair.  `None` selects
/// the entire file.  Out-of-range bounds are clamped to the file length.
#[allow(dead_code)]
pub fn run_binary(
    file: &Path,
    byte_range: Option<(u64, u64)>,
    out: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let all_bytes = fs::read(file)?;

    let slice = match byte_range {
        None => &all_bytes[..],
        Some((start, end)) => {
            // Convert 1-based inclusive to 0-based exclusive.
            let lo = (start.saturating_sub(1) as usize).min(all_bytes.len());
            let hi = (end as usize).min(all_bytes.len());
            &all_bytes[lo..hi]
        }
    };

    let escaped = crate::escape::encode_bytes(slice);
    out.write_all(escaped.as_bytes())?;
    Ok(())
}

/// Parse a `--bytes` argument string into a 1-based inclusive range.
///
/// Accepts:
/// - `"N"` — single byte N.
/// - `"N-M"` — bytes N through M inclusive.
///
/// Returns `Err` with a human-readable message on invalid input.
pub fn parse_bytes_arg(s: &str) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    if let Some((lo, hi)) = s.split_once('-') {
        let lo: u64 = lo
            .trim()
            .parse()
            .map_err(|_| format!("invalid byte offset {:?} in --bytes", lo))?;
        let hi: u64 = hi
            .trim()
            .parse()
            .map_err(|_| format!("invalid byte offset {:?} in --bytes", hi))?;
        if lo == 0 || hi == 0 {
            return Err("--bytes: byte offsets are 1-based (minimum 1)".into());
        }
        if lo > hi {
            return Err(format!("--bytes: start {lo} is after end {hi}").into());
        }
        Ok((lo, hi))
    } else {
        let n: u64 = s
            .trim()
            .parse()
            .map_err(|_| format!("invalid byte offset {s:?} in --bytes"))?;
        if n == 0 {
            return Err("--bytes: byte offsets are 1-based (minimum 1)".into());
        }
        Ok((n, n))
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// --hash argument parsing
// ──────────────────────────────────────────────────────────────────────────────

/// The hash algorithm for a `--hash` argument.
///
/// Changing the string representation of any variant is a breaking change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashAlgo {
    Crc32,
    Md5,
}

/// The end bound of a `--hash` range.
///
/// `Eof` is the `$` / `EOF` sentinel that resolves to the file's byte length
/// at hash-computation time.
#[derive(Clone, Copy, Debug)]
pub enum HashRangeEnd {
    /// Absolute 0-based exclusive byte offset.
    Absolute(u64),
    /// End-of-file sentinel (`$` or `EOF` in the argument string).
    Eof,
}

/// The result of computing a single `--hash` entry over a resolved byte range.
pub struct ComputedHash {
    /// The hash algorithm used.
    pub algo: HashAlgo,
    /// 0-based inclusive start offset (same as the parsed spec).
    pub start: u64,
    /// Resolved 0-based exclusive end offset (EOF sentinel already substituted).
    pub resolved_end: u64,
    /// Lowercase hex digest: 8 chars for CRC32, 32 chars for MD5.
    pub hex_value: String,
}

/// Compute hashes for every spec in `specs` over the given byte slice.
///
/// `file_len` is the total length of the file; it is used both to resolve
/// the `Eof` sentinel and to validate that no hash range exceeds the file.
///
/// Returns `Err` with a clear message if any range is out of bounds or
/// inverted.  Hashes are returned in the same order as `specs`.
pub fn compute_hashes(
    all_bytes: &[u8],
    specs: &[HashSpec],
) -> Result<Vec<ComputedHash>, Box<dyn std::error::Error>> {
    let file_len = all_bytes.len() as u64;
    let mut results = Vec::with_capacity(specs.len());

    for spec in specs {
        let resolved_end = match spec.end {
            HashRangeEnd::Eof => file_len,
            HashRangeEnd::Absolute(e) => e,
        };

        if spec.start > resolved_end {
            return Err(
                format!("--hash: start {} exceeds end {}", spec.start, resolved_end).into(),
            );
        }
        if resolved_end > file_len {
            return Err(format!(
                "--hash: end {} exceeds file length {}",
                resolved_end, file_len
            )
            .into());
        }

        let slice = &all_bytes[spec.start as usize..resolved_end as usize];

        let hex_value = match spec.algo {
            HashAlgo::Crc32 => {
                let v = crc32fast::hash(slice);
                format!("{v:08x}")
            }
            HashAlgo::Md5 => {
                let digest = Md5::digest(slice);
                format!("{digest:x}")
            }
        };

        results.push(ComputedHash {
            algo: spec.algo,
            start: spec.start,
            resolved_end,
            hex_value,
        });
    }

    Ok(results)
}

/// A fully parsed `--hash <algo>:<start>-<end>` argument.
///
/// `start` and `end` use 0-based byte offsets and a half-open interval
/// `[start, end)`, matching the convention used by the `--validate` selectors.
pub struct HashSpec {
    pub algo: HashAlgo,
    /// 0-based inclusive start byte offset.
    pub start: u64,
    /// Exclusive end offset, or `Eof` to use the file's byte length.
    pub end: HashRangeEnd,
}

/// Parse a decimal or `0x`/`0X`-prefixed hexadecimal integer.
fn parse_hex_or_dec(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|_| format!("invalid hex offset {s:?}"))
    } else {
        s.parse::<u64>()
            .map_err(|_| format!("invalid byte offset {s:?}"))
    }
}

/// Parse a `--hash <algo>:<start>-<end>` argument string into a [`HashSpec`].
///
/// Accepts:
/// - `algo` — `crc32` or `md5` (case-sensitive).
/// - `start` — decimal or `0x`-prefixed hex 0-based byte offset.
/// - `end`   — decimal or `0x`-prefixed hex 0-based byte offset, or `$` / `EOF`
///   (case-insensitive) to mean end-of-file.
///
/// Returns `Err` with a human-readable message on invalid input.
pub fn parse_hash_arg(s: &str) -> Result<HashSpec, Box<dyn std::error::Error>> {
    let (algo_str, range_str) = s
        .split_once(':')
        .ok_or_else(|| format!("--hash: expected '<algo>:<start>-<end>', got {s:?}"))?;

    let algo = match algo_str.trim() {
        "crc32" => HashAlgo::Crc32,
        "md5" => HashAlgo::Md5,
        other => {
            return Err(
                format!("--hash: unknown algorithm {other:?}; expected 'crc32' or 'md5'").into(),
            );
        }
    };

    let (start_str, end_str) = range_str
        .split_once('-')
        .ok_or_else(|| format!("--hash: expected '<start>-<end>' in range, got {range_str:?}"))?;

    let start = parse_hex_or_dec(start_str).map_err(|e| format!("--hash start: {e}"))?;

    let end_trimmed = end_str.trim();
    let end = if end_trimmed == "$" || end_trimmed.eq_ignore_ascii_case("eof") {
        HashRangeEnd::Eof
    } else {
        HashRangeEnd::Absolute(
            parse_hex_or_dec(end_trimmed).map_err(|e| format!("--hash end: {e}"))?,
        )
    };

    Ok(HashSpec { algo, start, end })
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Write as IoWrite;

    use tempfile::NamedTempFile;

    use super::*;

    /// Write `content` to a temp file, call `run_binary`, and return the
    /// captured output bytes.
    fn binary_read(content: &[u8], byte_range: Option<(u64, u64)>) -> Vec<u8> {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        let mut out: Vec<u8> = Vec::new();
        run_binary(f.path(), byte_range, &mut out).unwrap();
        out
    }

    // ── run_binary: normal cases ──────────────────────────────────────────────

    #[test]
    fn binary_read_empty_file() {
        let mut f = NamedTempFile::new().unwrap();
        f.flush().unwrap();
        let mut out = Vec::new();
        run_binary(f.path(), None, &mut out).unwrap();
        assert_eq!(out, b"");
    }

    #[test]
    fn binary_read_plain_ascii() {
        assert_eq!(binary_read(b"hello", None), b"hello");
    }

    #[test]
    fn binary_read_no_trailing_newline() {
        // run_binary never appends a trailing newline.
        let out = binary_read(b"abc", None);
        assert!(!out.ends_with(b"\n"), "unexpected trailing newline");
    }

    #[test]
    fn binary_read_high_bytes_escaped() {
        assert_eq!(binary_read(b"\xFF\x00\xAB", None), b"\\xff\\0\\xab");
    }

    #[test]
    fn binary_read_lf_escaped() {
        assert_eq!(binary_read(b"a\nb", None), b"a\\nb");
    }

    #[test]
    fn binary_read_backslash_doubled() {
        assert_eq!(binary_read(b"a\\b", None), b"a\\\\b");
    }

    #[test]
    fn binary_read_full_range_equivalent_to_none() {
        let content = b"abcde";
        assert_eq!(
            binary_read(content, Some((1, 5))),
            binary_read(content, None)
        );
    }

    #[test]
    fn binary_read_partial_range() {
        assert_eq!(binary_read(b"abcde", Some((2, 4))), b"bcd");
    }

    #[test]
    fn binary_read_single_byte() {
        assert_eq!(binary_read(b"xyz", Some((2, 2))), b"y");
    }

    #[test]
    fn binary_read_range_clamps_to_file_end() {
        assert_eq!(binary_read(b"abc", Some((2, 999))), b"bc");
    }

    #[test]
    fn binary_read_range_start_beyond_file_is_empty() {
        assert_eq!(binary_read(b"abc", Some((10, 20))), b"");
    }

    #[test]
    fn binary_read_all_256_bytes_round_trip() {
        let all: Vec<u8> = (0u8..=255u8).collect();
        let out = binary_read(&all, None);
        let s = String::from_utf8(out).unwrap();
        let decoded = crate::escape::decode_bytes(&s).unwrap();
        assert_eq!(decoded, all);
    }

    // ── parse_bytes_arg: normal and edge cases ────────────────────────────────

    #[test]
    fn parse_bytes_single() {
        assert_eq!(parse_bytes_arg("5").unwrap(), (5, 5));
    }

    #[test]
    fn parse_bytes_range() {
        assert_eq!(parse_bytes_arg("3-7").unwrap(), (3, 7));
    }

    #[test]
    fn parse_bytes_range_with_spaces() {
        assert_eq!(parse_bytes_arg(" 2 - 9 ").unwrap(), (2, 9));
    }

    #[test]
    fn parse_bytes_minimum_valid() {
        assert_eq!(parse_bytes_arg("1").unwrap(), (1, 1));
    }

    #[test]
    fn parse_bytes_zero_is_error() {
        assert!(parse_bytes_arg("0").is_err());
    }

    #[test]
    fn parse_bytes_start_after_end_is_error() {
        assert!(parse_bytes_arg("10-5").is_err());
    }

    #[test]
    fn parse_bytes_non_numeric_is_error() {
        assert!(parse_bytes_arg("abc").is_err());
    }

    #[test]
    fn parse_bytes_zero_start_in_range_is_error() {
        assert!(parse_bytes_arg("0-5").is_err());
    }

    // ── parse_hash_arg: normal cases ──────────────────────────────────────────

    fn algo_is_crc32(spec: &HashSpec) -> bool {
        matches!(spec.algo, HashAlgo::Crc32)
    }

    fn algo_is_md5(spec: &HashSpec) -> bool {
        matches!(spec.algo, HashAlgo::Md5)
    }

    fn end_is_eof(spec: &HashSpec) -> bool {
        matches!(spec.end, HashRangeEnd::Eof)
    }

    fn end_absolute(spec: &HashSpec) -> u64 {
        match spec.end {
            HashRangeEnd::Absolute(v) => v,
            HashRangeEnd::Eof => panic!("expected Absolute, got Eof"),
        }
    }

    #[test]
    fn parse_hash_crc32_zero_to_eof_dollar() {
        let spec = parse_hash_arg("crc32:0-$").unwrap();
        assert!(algo_is_crc32(&spec));
        assert_eq!(spec.start, 0);
        assert!(end_is_eof(&spec));
    }

    #[test]
    fn parse_hash_crc32_zero_to_eof_keyword() {
        let spec = parse_hash_arg("crc32:0-EOF").unwrap();
        assert!(algo_is_crc32(&spec));
        assert_eq!(spec.start, 0);
        assert!(end_is_eof(&spec));
    }

    #[test]
    fn parse_hash_eof_keyword_case_insensitive() {
        // "eof" and "Eof" must also be accepted.
        let lower = parse_hash_arg("crc32:0-eof").unwrap();
        assert!(end_is_eof(&lower));
        let mixed = parse_hash_arg("md5:0-Eof").unwrap();
        assert!(end_is_eof(&mixed));
    }

    #[test]
    fn parse_hash_md5_decimal_range() {
        let spec = parse_hash_arg("md5:0-512").unwrap();
        assert!(algo_is_md5(&spec));
        assert_eq!(spec.start, 0);
        assert_eq!(end_absolute(&spec), 512);
    }

    #[test]
    fn parse_hash_crc32_hex_range() {
        let spec = parse_hash_arg("crc32:0x100-0x200").unwrap();
        assert!(algo_is_crc32(&spec));
        assert_eq!(spec.start, 0x100);
        assert_eq!(end_absolute(&spec), 0x200);
    }

    #[test]
    fn parse_hash_hex_uppercase_prefix() {
        let spec = parse_hash_arg("md5:0X10-0XFF").unwrap();
        assert!(algo_is_md5(&spec));
        assert_eq!(spec.start, 0x10);
        assert_eq!(end_absolute(&spec), 0xFF);
    }

    #[test]
    fn parse_hash_nonzero_start_decimal() {
        let spec = parse_hash_arg("crc32:128-256").unwrap();
        assert_eq!(spec.start, 128);
        assert_eq!(end_absolute(&spec), 256);
    }

    #[test]
    fn parse_hash_start_can_be_zero() {
        // 0 is a valid 0-based start offset for --hash (unlike --bytes which is 1-based).
        let spec = parse_hash_arg("crc32:0-64").unwrap();
        assert_eq!(spec.start, 0);
        assert_eq!(end_absolute(&spec), 64);
    }

    #[test]
    fn parse_hash_single_byte_range() {
        // start == end is valid syntax (hash of one byte).
        let spec = parse_hash_arg("md5:5-6").unwrap();
        assert_eq!(spec.start, 5);
        assert_eq!(end_absolute(&spec), 6);
    }

    #[test]
    fn parse_hash_large_hex_offset() {
        let spec = parse_hash_arg("crc32:0-0xFFFFFFFF").unwrap();
        assert_eq!(end_absolute(&spec), 0xFFFF_FFFF);
    }

    // ── parse_hash_arg: error cases ───────────────────────────────────────────

    #[test]
    fn parse_hash_missing_colon_is_error() {
        assert!(parse_hash_arg("crc32|0-EOF").is_err());
    }

    #[test]
    fn parse_hash_unknown_algo_is_error() {
        assert!(parse_hash_arg("sha256:0-EOF").is_err());
    }

    #[test]
    fn parse_hash_missing_range_separator_is_error() {
        assert!(parse_hash_arg("crc32:0").is_err());
    }

    #[test]
    fn parse_hash_non_numeric_start_is_error() {
        assert!(parse_hash_arg("crc32:abc-EOF").is_err());
    }

    #[test]
    fn parse_hash_non_numeric_end_is_error() {
        assert!(parse_hash_arg("crc32:0-xyz").is_err());
    }

    #[test]
    fn parse_hash_empty_algo_is_error() {
        assert!(parse_hash_arg(":0-EOF").is_err());
    }

    #[test]
    fn parse_hash_empty_string_is_error() {
        assert!(parse_hash_arg("").is_err());
    }

    // ── compute_hashes: helpers ───────────────────────────────────────────────

    /// Build a single-spec `&[HashSpec]` from a string and run `compute_hashes`.
    fn hash_one(data: &[u8], spec_str: &str) -> Result<ComputedHash, Box<dyn std::error::Error>> {
        let spec = parse_hash_arg(spec_str)?;
        let mut results = compute_hashes(data, &[spec])?;
        Ok(results.remove(0))
    }

    // ── compute_hashes: CRC32 ─────────────────────────────────────────────────

    #[test]
    fn compute_crc32_empty_slice() {
        // CRC32 of empty input is the well-known value 0x00000000.
        let h = hash_one(b"", "crc32:0-0").unwrap();
        assert_eq!(h.hex_value, "00000000");
    }

    #[test]
    fn compute_crc32_empty_via_eof() {
        // EOF sentinel on empty file also resolves to range 0-0.
        let h = hash_one(b"", "crc32:0-EOF").unwrap();
        assert_eq!(h.hex_value, "00000000");
        assert_eq!(h.resolved_end, 0);
    }

    #[test]
    fn compute_crc32_known_hello() {
        // crc32("hello") == 3610a686
        let h = hash_one(b"hello", "crc32:0-EOF").unwrap();
        assert_eq!(h.hex_value, "3610a686");
        assert_eq!(h.resolved_end, 5);
    }

    #[test]
    fn compute_crc32_prefix_subset() {
        // crc32("hel") == e50bf11b
        let h = hash_one(b"hello", "crc32:0-3").unwrap();
        assert_eq!(h.hex_value, "e50bf11b");
        assert_eq!(h.start, 0);
        assert_eq!(h.resolved_end, 3);
    }

    #[test]
    fn compute_crc32_single_null_byte() {
        // crc32([0x00]) == d202ef8d
        let h = hash_one(&[0x00], "crc32:0-1").unwrap();
        assert_eq!(h.hex_value, "d202ef8d");
    }

    #[test]
    fn compute_crc32_output_is_exactly_8_hex_chars() {
        let h = hash_one(b"hello", "crc32:0-5").unwrap();
        assert_eq!(h.hex_value.len(), 8, "CRC32 hex must be exactly 8 chars");
    }

    #[test]
    fn compute_crc32_output_is_lowercase() {
        // Result must be lowercase hex (no A-F).
        let h = hash_one(b"hello", "crc32:0-5").unwrap();
        assert_eq!(h.hex_value, h.hex_value.to_lowercase());
    }

    // ── compute_hashes: MD5 ──────────────────────────────────────────────────

    #[test]
    fn compute_md5_empty_slice() {
        // MD5 of empty input is the well-known value.
        let h = hash_one(b"", "md5:0-0").unwrap();
        assert_eq!(h.hex_value, "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn compute_md5_empty_via_eof() {
        let h = hash_one(b"", "md5:0-EOF").unwrap();
        assert_eq!(h.hex_value, "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn compute_md5_known_hello() {
        // md5("hello") == 5d41402abc4b2a76b9719d911017c592
        let h = hash_one(b"hello", "md5:0-EOF").unwrap();
        assert_eq!(h.hex_value, "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn compute_md5_suffix_subset() {
        // md5("llo") — bytes [2,5) of "hello"
        let h = hash_one(b"hello", "md5:2-5").unwrap();
        assert_eq!(h.hex_value, "7062da7393ecc31c3c0564020f85efd1");
        assert_eq!(h.start, 2);
        assert_eq!(h.resolved_end, 5);
    }

    #[test]
    fn compute_md5_single_null_byte() {
        // md5([0x00]) == 93b885adfe0da089cdf634904fd59f71
        let h = hash_one(&[0x00], "md5:0-1").unwrap();
        assert_eq!(h.hex_value, "93b885adfe0da089cdf634904fd59f71");
    }

    #[test]
    fn compute_md5_output_is_exactly_32_hex_chars() {
        let h = hash_one(b"hello", "md5:0-5").unwrap();
        assert_eq!(h.hex_value.len(), 32, "MD5 hex must be exactly 32 chars");
    }

    #[test]
    fn compute_md5_output_is_lowercase() {
        let h = hash_one(b"hello", "md5:0-5").unwrap();
        assert_eq!(h.hex_value, h.hex_value.to_lowercase());
    }

    // ── compute_hashes: EOF sentinel ─────────────────────────────────────────

    #[test]
    fn compute_eof_sentinel_resolves_to_file_length() {
        let data = b"abcde";
        let h = hash_one(data, "crc32:0-EOF").unwrap();
        assert_eq!(h.resolved_end, 5);
    }

    #[test]
    fn compute_eof_sentinel_dollar_sign() {
        let data = b"abcde";
        let h = hash_one(data, "crc32:0-$").unwrap();
        assert_eq!(h.resolved_end, 5);
    }

    #[test]
    fn compute_eof_and_absolute_produce_same_digest() {
        let data = b"hello";
        let eof = hash_one(data, "crc32:0-EOF").unwrap();
        let abs = hash_one(data, "crc32:0-5").unwrap();
        assert_eq!(eof.hex_value, abs.hex_value);
        assert_eq!(eof.resolved_end, abs.resolved_end);
    }

    // ── compute_hashes: multiple specs ───────────────────────────────────────

    #[test]
    fn compute_multiple_specs_in_order() {
        let data = b"hello";
        let specs: Vec<HashSpec> = ["crc32:0-EOF", "md5:0-EOF"]
            .iter()
            .map(|s| parse_hash_arg(s).unwrap())
            .collect();
        let results = compute_hashes(data, &specs).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].algo, HashAlgo::Crc32);
        assert_eq!(results[0].hex_value, "3610a686");
        assert_eq!(results[1].algo, HashAlgo::Md5);
        assert_eq!(results[1].hex_value, "5d41402abc4b2a76b9719d911017c592");
    }

    #[test]
    fn compute_empty_specs_returns_empty_vec() {
        let results = compute_hashes(b"hello", &[]).unwrap();
        assert!(results.is_empty());
    }

    // ── compute_hashes: out-of-range errors ──────────────────────────────────

    #[test]
    fn compute_end_exceeds_file_length_is_error() {
        // end=6 but file is only 5 bytes.
        let spec = parse_hash_arg("crc32:0-6").unwrap();
        assert!(compute_hashes(b"hello", &[spec]).is_err());
    }

    #[test]
    fn compute_start_exceeds_file_length_is_error() {
        // start=6 > file_len=5; also start > end (both wrong).
        let spec = parse_hash_arg("crc32:6-6").unwrap();
        assert!(compute_hashes(b"hello", &[spec]).is_err());
    }

    #[test]
    fn compute_start_after_end_is_error() {
        // start=3, end=2: inverted range.
        let spec = parse_hash_arg("crc32:3-2").unwrap();
        // parse_hash_arg allows this (range checking is at compute time).
        assert!(compute_hashes(b"hello", &[spec]).is_err());
    }

    #[test]
    fn compute_end_exactly_at_file_length_is_ok() {
        // end == file_len is valid (exclusive end convention).
        let spec = parse_hash_arg("crc32:0-5").unwrap();
        assert!(compute_hashes(b"hello", &[spec]).is_ok());
    }

    #[test]
    fn compute_start_equals_end_is_ok_empty_slice() {
        // start==end yields CRC32 of empty slice.
        let spec = parse_hash_arg("crc32:2-2").unwrap();
        let results = compute_hashes(b"hello", &[spec]).unwrap();
        assert_eq!(results[0].hex_value, "00000000");
    }

    #[test]
    fn compute_first_bad_spec_aborts_before_second() {
        // First spec is out of range; second would be fine. Both should fail.
        let specs: Vec<HashSpec> = [
            "crc32:0-99", // exceeds file
            "crc32:0-5",  // would be fine alone
        ]
        .iter()
        .map(|s| parse_hash_arg(s).unwrap())
        .collect();
        assert!(compute_hashes(b"hello", &specs).is_err());
    }
}
