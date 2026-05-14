// Copyright (c) 2026, Michael Grier

//! Pre-write validation guards for `--validate SELECTOR VALUE`.
//!
//! All validators run against the target file *before* any write takes place.
//! The first failure stops further checks and returns an error; the caller is
//! responsible for leaving the file unchanged.
//!
//! Selector prefixes and their modes:
//!
//! | Selector              | Mode   | Description                                      |
//! |---|---|---|
//! | `line:N`              | text   | line N (1-based) must exactly equal VALUE         |
//! | `line-contains:N`     | text   | line N must contain VALUE as a substring          |
//! | `bytes:OFFSET-END`    | binary | `[OFFSET, END)` must equal VALUE (contiguous hex) |
//! | `md5:OFFSET-END`      | binary | MD5 of `[OFFSET, END)` must equal VALUE (32 hex) |
//! | `crc32:OFFSET-END`    | binary | CRC32 of `[OFFSET, END)` must equal VALUE (8 hex) |
//!
//! OFFSET and END are decimal integers or `0x`/`0X`-prefixed hex.
//! Changing any selector prefix is a breaking CLI change.

use std::{error::Error, path::Path, sync::Arc};

use harrier::{encoding::SourceConfig, source::Source};
use md5::{Digest, Md5};

use crate::IoMode;

// ── Selector type ─────────────────────────────────────────────────────────────

/// A parsed `--validate` selector identifying what to check in the target file.
///
/// Numeric parameters are 0-based for byte offsets and 1-based for line
/// numbers, matching the documented CLI interface.  Changing variant meanings
/// or associated values is a breaking change.
pub enum ValidateSelector {
    /// `line:N` — line N (1-based) must exactly equal VALUE.
    Line(usize),
    /// `line-contains:N` — line N (1-based) must contain VALUE as a substring.
    LineContains(usize),
    /// `bytes:OFFSET-END` — byte range `[OFFSET, END)` (0-based, exclusive
    /// end) must equal VALUE decoded from a contiguous hex string.
    Bytes(usize, usize),
    /// `md5:OFFSET-END` — MD5 of `[OFFSET, END)` must equal VALUE (32
    /// lowercase hex chars).
    Md5(usize, usize),
    /// `crc32:OFFSET-END` — CRC32 of `[OFFSET, END)` must equal VALUE (8
    /// lowercase hex chars).
    Crc32(usize, usize),
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Parse a `--validate SELECTOR` string into a [`ValidateSelector`].
///
/// `is_binary` reflects whether `--binary` was given to the enclosing
/// subcommand.  Text selectors (`line:`, `line-contains:`) are rejected when
/// `is_binary` is true; binary selectors (`bytes:`, `md5:`, `crc32:`) are
/// rejected when `is_binary` is false.
pub fn parse_selector(selector: &str, is_binary: bool) -> Result<ValidateSelector, Box<dyn Error>> {
    // Try `line-contains:` before `line:` so the longer prefix wins.
    if let Some(rest) = selector.strip_prefix("line-contains:") {
        if is_binary {
            return Err(format!(
                "--validate: text selector {selector:?} cannot be used with --binary"
            )
            .into());
        }
        let n = parse_line_number(rest).map_err(|e| format!("--validate {selector:?}: {e}"))?;
        Ok(ValidateSelector::LineContains(n))
    } else if let Some(rest) = selector.strip_prefix("line:") {
        if is_binary {
            return Err(format!(
                "--validate: text selector {selector:?} cannot be used with --binary"
            )
            .into());
        }
        let n = parse_line_number(rest).map_err(|e| format!("--validate {selector:?}: {e}"))?;
        Ok(ValidateSelector::Line(n))
    } else if let Some(rest) = selector.strip_prefix("bytes:") {
        if !is_binary {
            return Err(
                format!("--validate: binary selector {selector:?} requires --binary").into(),
            );
        }
        let (lo, hi) =
            parse_offset_end(rest).map_err(|e| format!("--validate {selector:?}: {e}"))?;
        Ok(ValidateSelector::Bytes(lo, hi))
    } else if let Some(rest) = selector.strip_prefix("md5:") {
        if !is_binary {
            return Err(
                format!("--validate: binary selector {selector:?} requires --binary").into(),
            );
        }
        let (lo, hi) =
            parse_offset_end(rest).map_err(|e| format!("--validate {selector:?}: {e}"))?;
        Ok(ValidateSelector::Md5(lo, hi))
    } else if let Some(rest) = selector.strip_prefix("crc32:") {
        if !is_binary {
            return Err(
                format!("--validate: binary selector {selector:?} requires --binary").into(),
            );
        }
        let (lo, hi) =
            parse_offset_end(rest).map_err(|e| format!("--validate {selector:?}: {e}"))?;
        Ok(ValidateSelector::Crc32(lo, hi))
    } else {
        Err(format!("--validate: unknown selector {selector:?}").into())
    }
}

/// Run all `--validate` checks before a write operation.
///
/// `pairs` is a flat slice of alternating SELECTOR, VALUE strings as
/// collected by clap's `num_args = 2, action = Append` configuration.
/// Its length must be even.
///
/// All selectors must be compatible with `is_binary`.  Validation stops at
/// the first failure and returns an error so the caller can abort the write
/// without touching the file.
pub fn run_all(
    pairs: &[String],
    file: &Path,
    is_binary: bool,
    io_mode: IoMode,
) -> Result<(), Box<dyn Error>> {
    debug_assert_eq!(pairs.len() % 2, 0, "validate pairs must be even-length");
    for chunk in pairs.chunks(2) {
        let selector_str = &chunk[0];
        let value = &chunk[1];
        let sel = parse_selector(selector_str, is_binary)?;
        match &sel {
            ValidateSelector::Line(_) | ValidateSelector::LineContains(_) => {
                run_text_validator(&sel, value, file, io_mode)?;
            }
            ValidateSelector::Bytes(_, _)
            | ValidateSelector::Md5(_, _)
            | ValidateSelector::Crc32(_, _) => {
                run_binary_validator(&sel, value, file, io_mode)?;
            }
        }
    }
    Ok(())
}

// ── Text validators ───────────────────────────────────────────────────────────

fn run_text_validator(
    sel: &ValidateSelector,
    value: &str,
    file: &Path,
    io_mode: IoMode,
) -> Result<(), Box<dyn Error>> {
    let lines = decode_file_lines(file, io_mode)?;
    match sel {
        ValidateSelector::Line(n) => {
            let idx = n
                .checked_sub(1)
                .ok_or("--validate line:0: line numbers are 1-based")?;
            let line = lines.get(idx).ok_or_else(|| {
                format!(
                    "--validate line:{n}: file has only {} line{}",
                    lines.len(),
                    if lines.len() == 1 { "" } else { "s" }
                )
            })?;
            if line.as_str() != value {
                return Err(
                    format!("--validate line:{n}: expected {value:?}, found {line:?}").into(),
                );
            }
        }
        ValidateSelector::LineContains(n) => {
            let idx = n
                .checked_sub(1)
                .ok_or("--validate line-contains:0: line numbers are 1-based")?;
            let line = lines.get(idx).ok_or_else(|| {
                format!(
                    "--validate line-contains:{n}: file has only {} line{}",
                    lines.len(),
                    if lines.len() == 1 { "" } else { "s" }
                )
            })?;
            if !line.contains(value) {
                return Err(format!(
                    "--validate line-contains:{n}: {value:?} not found in {line:?}"
                )
                .into());
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

/// Open `file` with harrier, decode it to UTF-8, and return its lines.
///
/// Lines are split on the normalised LF boundaries that harrier produces.
/// The BOM (if present) is excluded from the content.  A trailing empty
/// string that would result from a terminal newline is dropped so that
/// line counts match what an editor would report.
fn decode_file_lines(file: &Path, io_mode: IoMode) -> Result<Vec<String>, Box<dyn Error>> {
    let branch = crate::open_as_branch(file, io_mode)?;
    let file_len = branch.byte_len();
    let source = Source::new(Arc::clone(&branch), SourceConfig::default())?;
    let bom_len = source.bom_len();
    let encoding = source.encoding();
    let lines = source.as_lines()?;
    let view = lines.view_range(bom_len as u64..file_len)?;
    let (text, _) = encoding.decode_without_bom_handling(&view.bytes);

    let mut parts: Vec<String> = text.split('\n').map(str::to_owned).collect();
    // Drop the trailing empty element produced by a terminal '\n'.
    if parts.last().map(String::is_empty).unwrap_or(false) {
        parts.pop();
    }
    Ok(parts)
}

// ── Binary validators ─────────────────────────────────────────────────────────

fn run_binary_validator(
    sel: &ValidateSelector,
    value: &str,
    file: &Path,
    io_mode: IoMode,
) -> Result<(), Box<dyn Error>> {
    let bytes = crate::read_raw_bytes(file, io_mode)?;

    match sel {
        ValidateSelector::Bytes(lo, hi) => {
            let slice = byte_slice(&bytes, *lo, *hi)?;
            let expected = parse_hex_bytes(value)?;
            if slice != expected.as_slice() {
                return Err(format!(
                    "--validate bytes:{lo}-{hi}: content mismatch \
                     (expected {} bytes matching hex value)",
                    expected.len()
                )
                .into());
            }
        }
        ValidateSelector::Md5(lo, hi) => {
            let slice = byte_slice(&bytes, *lo, *hi)?;
            let digest = Md5::digest(slice);
            let actual = format!("{digest:x}");
            let expected = value.to_lowercase();
            if actual != expected {
                return Err(format!(
                    "--validate md5:{lo}-{hi}: expected {expected}, computed {actual}"
                )
                .into());
            }
        }
        ValidateSelector::Crc32(lo, hi) => {
            let slice = byte_slice(&bytes, *lo, *hi)?;
            let actual = crc32fast::hash(slice);
            let actual_hex = format!("{actual:08x}");
            let expected = value.to_lowercase();
            if actual_hex != expected {
                return Err(format!(
                    "--validate crc32:{lo}-{hi}: expected {expected}, computed {actual_hex}"
                )
                .into());
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

/// Slice `[lo, hi)` from a byte buffer, returning an error if the
/// range is out of bounds or inverted.
fn byte_slice(bytes: &[u8], lo: usize, hi: usize) -> Result<&[u8], Box<dyn Error>> {
    if lo > hi {
        return Err(format!("byte range {lo}-{hi}: start exceeds end").into());
    }
    if hi > bytes.len() {
        return Err(format!(
            "byte range {lo}-{hi}: end {} exceeds file length {}",
            hi,
            bytes.len()
        )
        .into());
    }
    Ok(&bytes[lo..hi])
}

// ── Common parsing helpers ────────────────────────────────────────────────────

/// Parse a 1-based line number from a decimal string.
fn parse_line_number(s: &str) -> Result<usize, Box<dyn Error>> {
    let n = s
        .parse::<usize>()
        .map_err(|_| format!("invalid line number {s:?}"))?;
    if n == 0 {
        return Err("line numbers are 1-based (0 is not valid)".into());
    }
    Ok(n)
}

/// Parse `OFFSET-END` where each part is decimal or `0x`/`0X`-prefixed hex.
///
/// Splits on the first `-` so that hex values containing no `-` are handled
/// correctly.  Neither OFFSET nor END may be negative.
fn parse_offset_end(s: &str) -> Result<(usize, usize), Box<dyn Error>> {
    let (lo_str, hi_str) = s
        .split_once('-')
        .ok_or_else(|| format!("expected OFFSET-END, got {s:?}"))?;
    let lo = parse_size(lo_str)?;
    let hi = parse_size(hi_str)?;
    Ok((lo, hi))
}

/// Parse a decimal or `0x`/`0X`-prefixed hex integer.
fn parse_size(s: &str) -> Result<usize, Box<dyn Error>> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        usize::from_str_radix(hex, 16).map_err(|_| format!("invalid hex value {s:?}").into())
    } else {
        s.parse::<usize>()
            .map_err(|_| format!("invalid decimal value {s:?}").into())
    }
}

/// Decode a contiguous hex string (no separators) into bytes.
fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    if !s.len().is_multiple_of(2) {
        return Err(format!("hex string has odd length: {s:?}").into());
    }
    (0..s.len() / 2)
        .map(|i| {
            u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|_| format!("invalid hex byte at position {i} in {s:?}").into())
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_size ────────────────────────────────────────────────────────────

    #[test]
    fn size_decimal_zero() {
        assert_eq!(parse_size("0").unwrap(), 0);
    }

    #[test]
    fn size_decimal() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
    }

    #[test]
    fn size_hex_lower() {
        assert_eq!(parse_size("0xff").unwrap(), 255);
    }

    #[test]
    fn size_hex_upper_prefix() {
        assert_eq!(parse_size("0XFF").unwrap(), 255);
    }

    #[test]
    fn size_hex_mixed_case_digits() {
        assert_eq!(parse_size("0x1A2b").unwrap(), 0x1A2B);
    }

    #[test]
    fn size_invalid_decimal() {
        assert!(parse_size("abc").is_err());
    }

    #[test]
    fn size_invalid_hex() {
        assert!(parse_size("0xGG").is_err());
    }

    #[test]
    fn size_empty() {
        assert!(parse_size("").is_err());
    }

    // ── parse_offset_end ──────────────────────────────────────────────────────

    #[test]
    fn offset_end_decimal() {
        assert_eq!(parse_offset_end("0-100").unwrap(), (0, 100));
    }

    #[test]
    fn offset_end_hex() {
        assert_eq!(parse_offset_end("0x0-0x64").unwrap(), (0, 100));
    }

    #[test]
    fn offset_end_mixed() {
        assert_eq!(parse_offset_end("0-0x64").unwrap(), (0, 100));
    }

    #[test]
    fn offset_end_same() {
        // lo == hi is valid (empty range)
        assert_eq!(parse_offset_end("10-10").unwrap(), (10, 10));
    }

    #[test]
    fn offset_end_missing_dash() {
        assert!(parse_offset_end("100").is_err());
    }

    #[test]
    fn offset_end_bad_lo() {
        assert!(parse_offset_end("x-100").is_err());
    }

    #[test]
    fn offset_end_bad_hi() {
        assert!(parse_offset_end("0-abc").is_err());
    }

    // ── parse_line_number ─────────────────────────────────────────────────────

    #[test]
    fn line_number_one() {
        assert_eq!(parse_line_number("1").unwrap(), 1);
    }

    #[test]
    fn line_number_large() {
        assert_eq!(parse_line_number("9999").unwrap(), 9999);
    }

    #[test]
    fn line_number_zero_rejected() {
        assert!(parse_line_number("0").is_err());
    }

    #[test]
    fn line_number_non_numeric() {
        assert!(parse_line_number("one").is_err());
    }

    // ── parse_hex_bytes ───────────────────────────────────────────────────────

    #[test]
    fn hex_bytes_empty() {
        assert_eq!(parse_hex_bytes("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hex_bytes_single() {
        assert_eq!(parse_hex_bytes("ff").unwrap(), vec![0xff]);
    }

    #[test]
    fn hex_bytes_multiple() {
        assert_eq!(parse_hex_bytes("4d5a00").unwrap(), vec![0x4d, 0x5a, 0x00]);
    }

    #[test]
    fn hex_bytes_uppercase() {
        assert_eq!(parse_hex_bytes("4D5A").unwrap(), vec![0x4d, 0x5a]);
    }

    #[test]
    fn hex_bytes_odd_length() {
        assert!(parse_hex_bytes("abc").is_err());
    }

    #[test]
    fn hex_bytes_invalid_chars() {
        assert!(parse_hex_bytes("GG").is_err());
    }

    // ── parse_selector ────────────────────────────────────────────────────────

    #[test]
    fn selector_line_text_mode() {
        let sel = parse_selector("line:3", false).unwrap();
        assert!(matches!(sel, ValidateSelector::Line(3)));
    }

    #[test]
    fn selector_line_binary_rejected() {
        assert!(parse_selector("line:3", true).is_err());
    }

    #[test]
    fn selector_line_contains_text_mode() {
        let sel = parse_selector("line-contains:1", false).unwrap();
        assert!(matches!(sel, ValidateSelector::LineContains(1)));
    }

    #[test]
    fn selector_line_contains_binary_rejected() {
        assert!(parse_selector("line-contains:2", true).is_err());
    }

    #[test]
    fn selector_bytes_binary_mode() {
        let sel = parse_selector("bytes:0-4", true).unwrap();
        assert!(matches!(sel, ValidateSelector::Bytes(0, 4)));
    }

    #[test]
    fn selector_bytes_text_rejected() {
        assert!(parse_selector("bytes:0-4", false).is_err());
    }

    #[test]
    fn selector_md5_binary_mode() {
        let sel = parse_selector("md5:0-100", true).unwrap();
        assert!(matches!(sel, ValidateSelector::Md5(0, 100)));
    }

    #[test]
    fn selector_md5_text_rejected() {
        assert!(parse_selector("md5:0-100", false).is_err());
    }

    #[test]
    fn selector_crc32_binary_mode() {
        let sel = parse_selector("crc32:0-100", true).unwrap();
        assert!(matches!(sel, ValidateSelector::Crc32(0, 100)));
    }

    #[test]
    fn selector_crc32_text_rejected() {
        assert!(parse_selector("crc32:0-100", false).is_err());
    }

    #[test]
    fn selector_unknown() {
        assert!(parse_selector("sha256:0-100", true).is_err());
    }

    #[test]
    fn selector_line_zero_is_error() {
        assert!(parse_selector("line:0", false).is_err());
    }

    #[test]
    fn selector_line_contains_hex_range() {
        // line-contains must take a line number, not a range
        assert!(parse_selector("line-contains:abc", false).is_err());
    }

    // ── parse_hex_bytes round-trip ─────────────────────────────────────────────

    #[test]
    fn hex_bytes_round_trip_known() {
        let data: &[u8] = &[0x00, 0x01, 0xFE, 0xFF];
        let hex = data.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(parse_hex_bytes(&hex).unwrap(), data);
    }

    // ── byte_slice ────────────────────────────────────────────────────────────

    #[test]
    fn byte_slice_out_of_bounds() {
        let bytes = b"hello";
        // hi exceeds length
        assert!(byte_slice(bytes, 0, 100).is_err());
    }

    #[test]
    fn byte_slice_inverted() {
        let bytes = b"hello";
        // lo > hi
        assert!(byte_slice(bytes, 3, 1).is_err());
    }

    #[test]
    fn byte_slice_valid() {
        let bytes = b"hello";
        assert_eq!(byte_slice(bytes, 1, 4).unwrap(), b"ell");
    }

    // ── Validator function tests (file-based) ─────────────────────────────────

    fn utf8_lf_file(content: &str) -> (tempfile::NamedTempFile, std::path::PathBuf) {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        let p = f.path().to_path_buf();
        (f, p)
    }

    fn binary_file(bytes: &[u8]) -> (tempfile::NamedTempFile, std::path::PathBuf) {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        let p = f.path().to_path_buf();
        (f, p)
    }

    // text: line:N exact match passes
    #[test]
    fn text_validator_line_exact_match_pass() {
        let (_f, path) = utf8_lf_file("hello\nworld\n");
        assert!(
            run_text_validator(&ValidateSelector::Line(1), "hello", &path, IoMode::Mmap).is_ok()
        );
    }

    // text: line:N exact mismatch gives error
    #[test]
    fn text_validator_line_exact_mismatch_fail() {
        let (_f, path) = utf8_lf_file("hello\nworld\n");
        assert!(
            run_text_validator(&ValidateSelector::Line(1), "world", &path, IoMode::Mmap).is_err()
        );
    }

    // text: line:N second line match passes
    #[test]
    fn text_validator_line_second_line_pass() {
        let (_f, path) = utf8_lf_file("alpha\nbeta\n");
        assert!(
            run_text_validator(&ValidateSelector::Line(2), "beta", &path, IoMode::Mmap).is_ok()
        );
    }

    // text: line:N out-of-range gives error
    #[test]
    fn text_validator_line_out_of_range() {
        let (_f, path) = utf8_lf_file("hello\nworld\n");
        assert!(
            run_text_validator(&ValidateSelector::Line(5), "anything", &path, IoMode::Mmap)
                .is_err()
        );
    }

    // text: line-contains:N substring found passes
    #[test]
    fn text_validator_line_contains_pass() {
        let (_f, path) = utf8_lf_file("hello world\n");
        assert!(
            run_text_validator(
                &ValidateSelector::LineContains(1),
                "world",
                &path,
                IoMode::Mmap
            )
            .is_ok()
        );
    }

    // text: line-contains:N full line also passes (substring)
    #[test]
    fn text_validator_line_contains_full_line_pass() {
        let (_f, path) = utf8_lf_file("hello\n");
        assert!(
            run_text_validator(
                &ValidateSelector::LineContains(1),
                "hello",
                &path,
                IoMode::Mmap
            )
            .is_ok()
        );
    }

    // text: line-contains:N substring not found gives error
    #[test]
    fn text_validator_line_contains_not_found() {
        let (_f, path) = utf8_lf_file("hello world\n");
        assert!(
            run_text_validator(
                &ValidateSelector::LineContains(1),
                "rust",
                &path,
                IoMode::Mmap
            )
            .is_err()
        );
    }

    // text: line-contains:N out-of-range gives error
    #[test]
    fn text_validator_line_contains_out_of_range() {
        let (_f, path) = utf8_lf_file("hello\n");
        assert!(
            run_text_validator(
                &ValidateSelector::LineContains(99),
                "hello",
                &path,
                IoMode::Mmap
            )
            .is_err()
        );
    }

    // binary: bytes:lo-hi exact match passes
    #[test]
    fn binary_validator_bytes_match_pass() {
        let data = b"hello";
        let (_f, path) = binary_file(data);
        let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
        assert!(
            run_binary_validator(&ValidateSelector::Bytes(0, 5), &hex, &path, IoMode::Mmap).is_ok()
        );
    }

    // binary: bytes:lo-hi middle range match passes
    #[test]
    fn binary_validator_bytes_middle_range_pass() {
        let data = b"abcde";
        let (_f, path) = binary_file(data);
        // b"bcd" = 62 63 64
        assert!(
            run_binary_validator(
                &ValidateSelector::Bytes(1, 4),
                "626364",
                &path,
                IoMode::Mmap
            )
            .is_ok()
        );
    }

    // binary: bytes:lo-hi mismatch gives error
    #[test]
    fn binary_validator_bytes_mismatch_fail() {
        let (_f, path) = binary_file(b"hello");
        assert!(
            run_binary_validator(
                &ValidateSelector::Bytes(0, 5),
                "0000000000",
                &path,
                IoMode::Mmap
            )
            .is_err()
        );
    }

    // binary: bytes:lo-hi beyond file end gives error
    #[test]
    fn binary_validator_bytes_out_of_range() {
        let (_f, path) = binary_file(b"hi");
        assert!(
            run_binary_validator(
                &ValidateSelector::Bytes(0, 100),
                "0000",
                &path,
                IoMode::Mmap
            )
            .is_err()
        );
    }

    // binary: md5:lo-hi correct digest passes
    #[test]
    fn binary_validator_md5_match_pass() {
        use md5::{Digest as _, Md5};
        let data = b"hello";
        let (_f, path) = binary_file(data);
        let expected = format!("{:x}", Md5::digest(data));
        assert!(
            run_binary_validator(&ValidateSelector::Md5(0, 5), &expected, &path, IoMode::Mmap)
                .is_ok()
        );
    }

    // binary: md5:lo-hi uppercase digest also passes (case-insensitive)
    #[test]
    fn binary_validator_md5_uppercase_accepted() {
        use md5::{Digest as _, Md5};
        let data = b"hello";
        let (_f, path) = binary_file(data);
        let expected = format!("{:x}", Md5::digest(data)).to_uppercase();
        assert!(
            run_binary_validator(&ValidateSelector::Md5(0, 5), &expected, &path, IoMode::Mmap)
                .is_ok()
        );
    }

    // binary: md5:lo-hi wrong digest gives error
    #[test]
    fn binary_validator_md5_mismatch_fail() {
        let (_f, path) = binary_file(b"hello");
        assert!(
            run_binary_validator(
                &ValidateSelector::Md5(0, 5),
                "00000000000000000000000000000000",
                &path,
                IoMode::Mmap,
            )
            .is_err()
        );
    }

    // binary: crc32:lo-hi correct checksum passes
    #[test]
    fn binary_validator_crc32_match_pass() {
        let data = b"hello";
        let (_f, path) = binary_file(data);
        let expected = format!("{:08x}", crc32fast::hash(data));
        assert!(
            run_binary_validator(
                &ValidateSelector::Crc32(0, 5),
                &expected,
                &path,
                IoMode::Mmap
            )
            .is_ok()
        );
    }

    // binary: crc32:lo-hi uppercase checksum also passes (case-insensitive)
    #[test]
    fn binary_validator_crc32_uppercase_accepted() {
        let data = b"hello";
        let (_f, path) = binary_file(data);
        let expected = format!("{:08x}", crc32fast::hash(data)).to_uppercase();
        assert!(
            run_binary_validator(
                &ValidateSelector::Crc32(0, 5),
                &expected,
                &path,
                IoMode::Mmap
            )
            .is_ok()
        );
    }

    // binary: crc32:lo-hi wrong checksum gives error
    #[test]
    fn binary_validator_crc32_mismatch_fail() {
        let (_f, path) = binary_file(b"hello");
        assert!(
            run_binary_validator(
                &ValidateSelector::Crc32(0, 5),
                "00000000",
                &path,
                IoMode::Mmap
            )
            .is_err()
        );
    }

    // run_all: two passing validators both succeed
    #[test]
    fn run_all_two_passing_validators() {
        let (_f, path) = utf8_lf_file("foo\nbar\n");
        let pairs = vec![
            "line:1".to_string(),
            "foo".to_string(),
            "line:2".to_string(),
            "bar".to_string(),
        ];
        assert!(run_all(&pairs, &path, false, IoMode::Mmap).is_ok());
    }

    // run_all: first validator fails, error is returned
    #[test]
    fn run_all_first_fails_returns_error() {
        let (_f, path) = utf8_lf_file("foo\nbar\n");
        let pairs = vec![
            "line:1".to_string(),
            "wrong".to_string(),
            "line:2".to_string(),
            "bar".to_string(),
        ];
        assert!(run_all(&pairs, &path, false, IoMode::Mmap).is_err());
    }

    // run_all: file not found gives error (no panic)
    #[test]
    fn run_all_file_not_found() {
        let pairs = vec!["line:1".to_string(), "hello".to_string()];
        let missing = std::path::Path::new("__no_such_file_validate_test__.txt");
        assert!(run_all(&pairs, missing, false, IoMode::Mmap).is_err());
    }
}
