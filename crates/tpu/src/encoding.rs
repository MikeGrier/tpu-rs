// Copyright (c) 2026, Michael Grier

//! Shared types for output encoding normalisation.
//!
//! These types are used by subcommands that support `--utf8` and `--bom`
//! options.  The default behaviour (when neither flag is supplied) is to
//! preserve the source file's encoding and BOM exactly as found.

use std::str::FromStr;

/// Re-export so consumers can name the line-ending type as
/// `tpu::encoding::LineEnding` (the canonical type used throughout `tpu`'s
/// public API for `--line-ending` overrides and git-EOL normalisation).
#[allow(unused_imports)]
// Re-exported for tpu-mcp (library consumer); unused inside the tpu binary.
pub use harrier::encoding::LineEnding;

/// Whether a subcommand should re-encode output as UTF-8.
///
/// The default is [`OutputEncoding::Preserve`]: the file's native encoding is
/// kept.  [`OutputEncoding::Utf8`] forces UTF-8 output; the companion
/// [`BomPolicy`] then governs whether a BOM byte sequence is included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputEncoding {
    /// Keep the source file's encoding unchanged (default).
    #[default]
    Preserve,
    /// Re-encode output as UTF-8.
    Utf8,
}

/// How to handle the UTF-8 byte-order mark when `--utf8` is active.
///
/// Has no effect when [`OutputEncoding::Preserve`] is in use.
///
/// # Changing this default is a breaking change
///
/// The default is `Strip` (no BOM).  Future changes to the default must be
/// documented in `DESIGN-NOTES.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BomPolicy {
    /// Do not write a BOM in the output (default).
    #[default]
    Strip,
    /// Write a BOM only if the source file contained one.
    Preserve,
    /// Always write a BOM regardless of the source.
    Force,
}

impl FromStr for BomPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "strip" => Ok(BomPolicy::Strip),
            "preserve" => Ok(BomPolicy::Preserve),
            "force" => Ok(BomPolicy::Force),
            other => Err(format!(
                "invalid --bom value {other:?}: expected strip, preserve, or force"
            )),
        }
    }
}

/// Parse a line-ending name (`"lf"`, `"crlf"`, `"cr"`) into a
/// [`harrier::encoding::LineEnding`] value.
///
/// This is the canonical parse function shared by `tpu` CLI and `tpu-mcp`.
/// Callers pass the raw string from a `--line-ending` flag or an MCP JSON
/// field.  Returns `Err` with a human-readable message on unrecognised input.
#[allow(dead_code)] // Used by tpu-mcp (library consumer), not by the tpu binary.
pub fn parse_line_ending(
    s: &str,
) -> Result<harrier::encoding::LineEnding, Box<dyn std::error::Error>> {
    match s {
        "lf" => Ok(harrier::encoding::LineEnding::Lf),
        "crlf" => Ok(harrier::encoding::LineEnding::CrLf),
        "cr" => Ok(harrier::encoding::LineEnding::Cr),
        other => Err(
            format!("unrecognised line-ending value {other:?}; expected lf, crlf, or cr").into(),
        ),
    }
}

// ── Line-ending denormalisation / normalisation (native byte space) ───────────
//
// These helpers operate directly on a file's *native* encoded bytes — they
// never decode to UTF-8 and re-encode.  That matters because:
//   * `encoding_rs` has no UTF-16 *encoder*: `Encoding::encode` silently falls
//     back to UTF-8 for UTF-16LE/BE, which would corrupt the stream.
//   * decode()/encode() round-trips strip (and fail to re-add) a leading BOM.
// Operating on native bytes leaves the BOM and every non-newline byte
// untouched, and keeps UTF-16 code units correctly aligned.
//
// Encoding-specific line-ending code units:
//   UTF-16LE  CR = [0x0D, 0x00]  LF = [0x0A, 0x00]  CRLF = [0x0D,0x00,0x0A,0x00]
//   UTF-16BE  CR = [0x00, 0x0D]  LF = [0x00, 0x0A]  CRLF = [0x00,0x0D,0x00,0x0A]
//   All else  CR = [0x0D]        LF = [0x0A]        CRLF = [0x0D, 0x0A]

/// Return the BOM byte sequence for `encoding`, or an empty slice if none.
pub(crate) fn bom_bytes_for(encoding: &'static encoding_rs::Encoding) -> &'static [u8] {
    if encoding == encoding_rs::UTF_8 {
        &[0xEF, 0xBB, 0xBF]
    } else if encoding == encoding_rs::UTF_16LE {
        &[0xFF, 0xFE]
    } else if encoding == encoding_rs::UTF_16BE {
        &[0xFE, 0xFF]
    } else {
        &[]
    }
}

/// Scan `bytes` in 2-byte (UTF-16 code-unit) steps, replacing every
/// occurrence of the 2-byte `needle` with `replacement`.
///
/// Both the scan and the forwarding advance a whole code unit (2 bytes) at a
/// time, so the `needle` is only ever matched on a code-unit boundary.  A
/// 1-byte scan would let `needle` straddle two adjacent units (e.g. the high
/// byte of one unit plus the low byte of the next) and corrupt the stream.
/// Any odd-length tail (which should not occur in valid UTF-16) is forwarded
/// rather than silently dropped.
pub(crate) fn replace_u16_pairs(bytes: &[u8], needle: [u8; 2], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 20);
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == needle[0] && bytes[i + 1] == needle[1] {
            out.extend_from_slice(replacement);
        } else {
            out.push(bytes[i]);
            out.push(bytes[i + 1]);
        }
        i += 2;
    }
    if i < bytes.len() {
        out.push(bytes[i]);
    }
    out
}

/// Insert `\r` (0x0D) before each `\n` (0x0A) byte in a single-byte or
/// multi-byte UTF-8 stream.
///
/// Only `\n` bytes are targeted; no byte in a valid UTF-8 or single-byte
/// encoding can be confused with `\n` (0x0A is never a continuation byte).
pub(crate) fn insert_cr_before_lf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 20);
    for &b in bytes {
        if b == 0x0A {
            out.push(0x0D);
        }
        out.push(b);
    }
    out
}

/// Convert LF-only native `bytes` to CRLF endings for `encoding`.
///
/// The input must already be LF-only (e.g. straight from
/// `Encoding::encode`), so there is no risk of double-inserting CR before an
/// existing CR.
pub(crate) fn denormalize_lf_to_crlf(
    bytes: &[u8],
    encoding: &'static encoding_rs::Encoding,
) -> Vec<u8> {
    if encoding == encoding_rs::UTF_16LE {
        replace_u16_pairs(bytes, [0x0A, 0x00], &[0x0D, 0x00, 0x0A, 0x00])
    } else if encoding == encoding_rs::UTF_16BE {
        replace_u16_pairs(bytes, [0x00, 0x0A], &[0x00, 0x0D, 0x00, 0x0A])
    } else {
        insert_cr_before_lf(bytes)
    }
}

/// Convert LF-only native `bytes` to lone-CR endings for `encoding`.
///
/// The input must already be LF-only (see [`denormalize_lf_to_crlf`]).
pub(crate) fn denormalize_lf_to_cr(
    bytes: &[u8],
    encoding: &'static encoding_rs::Encoding,
) -> Vec<u8> {
    if encoding == encoding_rs::UTF_16LE {
        replace_u16_pairs(bytes, [0x0A, 0x00], &[0x0D, 0x00])
    } else if encoding == encoding_rs::UTF_16BE {
        replace_u16_pairs(bytes, [0x00, 0x0A], &[0x00, 0x0D])
    } else {
        bytes
            .iter()
            .map(|&b| if b == 0x0A { 0x0D } else { b })
            .collect()
    }
}

/// Normalise CRLF and lone-CR endings to LF in a single-byte / UTF-8 stream.
///
/// `0x0D` / `0x0A` are unambiguous line-ending bytes in every non-UTF-16
/// encoding harrier detects (UTF-8 continuation bytes and Shift-JIS trailing
/// bytes never take those values), so a byte-level scan is safe.
pub(crate) fn normalize_bytes_to_lf(bytes: &[u8]) -> Vec<u8> {
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
pub(crate) fn normalize_u16_to_lf(bytes: &[u8], cr: [u8; 2], lf: [u8; 2]) -> Vec<u8> {
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

/// Rewrite every line ending in `bytes` (in the file's native `encoding`) to
/// `target`, leaving the BOM and all non-newline bytes untouched.
///
/// The input is first normalised to LF-only in native byte space, then the
/// `target` ending is applied.  No decode/re-encode round-trip is performed.
pub(crate) fn apply_line_ending_to_all(
    bytes: Vec<u8>,
    encoding: &'static encoding_rs::Encoding,
    target: LineEnding,
) -> Vec<u8> {
    if encoding == encoding_rs::UTF_16LE {
        let lf = normalize_u16_to_lf(&bytes, [0x0D, 0x00], [0x0A, 0x00]);
        match target {
            LineEnding::Lf => lf,
            LineEnding::CrLf => replace_u16_pairs(&lf, [0x0A, 0x00], &[0x0D, 0x00, 0x0A, 0x00]),
            LineEnding::Cr => replace_u16_pairs(&lf, [0x0A, 0x00], &[0x0D, 0x00]),
        }
    } else if encoding == encoding_rs::UTF_16BE {
        let lf = normalize_u16_to_lf(&bytes, [0x00, 0x0D], [0x00, 0x0A]);
        match target {
            LineEnding::Lf => lf,
            LineEnding::CrLf => replace_u16_pairs(&lf, [0x00, 0x0A], &[0x00, 0x0D, 0x00, 0x0A]),
            LineEnding::Cr => replace_u16_pairs(&lf, [0x00, 0x0A], &[0x00, 0x0D]),
        }
    } else {
        let lf = normalize_bytes_to_lf(&bytes);
        match target {
            LineEnding::Lf => lf,
            LineEnding::CrLf => insert_cr_before_lf(&lf),
            LineEnding::Cr => lf
                .iter()
                .map(|&b| if b == 0x0A { 0x0D } else { b })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom_policy_from_str_strip() {
        assert_eq!("strip".parse::<BomPolicy>().unwrap(), BomPolicy::Strip);
    }

    #[test]
    fn bom_policy_from_str_preserve() {
        assert_eq!(
            "preserve".parse::<BomPolicy>().unwrap(),
            BomPolicy::Preserve
        );
    }

    #[test]
    fn bom_policy_from_str_force() {
        assert_eq!("force".parse::<BomPolicy>().unwrap(), BomPolicy::Force);
    }

    #[test]
    fn bom_policy_from_str_invalid() {
        let err = "auto".parse::<BomPolicy>().unwrap_err();
        assert!(err.contains("invalid --bom value"));
        assert!(err.contains("\"auto\""));
    }

    #[test]
    fn output_encoding_default_is_preserve() {
        assert_eq!(OutputEncoding::default(), OutputEncoding::Preserve);
    }

    #[test]
    fn bom_policy_default_is_strip() {
        assert_eq!(BomPolicy::default(), BomPolicy::Strip);
    }

    #[test]
    fn bom_policy_debug_roundtrip_strip() {
        assert!(format!("{:?}", BomPolicy::Strip).contains("Strip"));
    }

    #[test]
    fn bom_policy_debug_roundtrip_preserve() {
        assert!(format!("{:?}", BomPolicy::Preserve).contains("Preserve"));
    }

    #[test]
    fn bom_policy_debug_roundtrip_force() {
        assert!(format!("{:?}", BomPolicy::Force).contains("Force"));
    }

    #[test]
    fn bom_policy_from_str_empty_string_is_error() {
        assert!("".parse::<BomPolicy>().is_err());
    }

    #[test]
    fn bom_policy_from_str_case_sensitive() {
        // Values must be lowercase; "Strip" (titlecase) is not accepted.
        assert!("Strip".parse::<BomPolicy>().is_err());
        assert!("STRIP".parse::<BomPolicy>().is_err());
    }

    #[test]
    fn output_encoding_variants_are_distinct() {
        assert_ne!(OutputEncoding::Preserve, OutputEncoding::Utf8);
    }

    // ── replace_u16_pairs (UTF-16 code-unit stepping) ────────────────────────

    #[test]
    fn replace_u16_pairs_replaces_aligned_needle() {
        // UTF-16LE: 'A' (U+0041) followed by an LF code unit (U+000A).
        let bytes = [0x41, 0x00, 0x0A, 0x00];
        let out = replace_u16_pairs(&bytes, [0x0A, 0x00], &[0x0D, 0x00, 0x0A, 0x00]);
        assert_eq!(out, [0x41, 0x00, 0x0D, 0x00, 0x0A, 0x00]);
    }

    #[test]
    fn replace_u16_pairs_ignores_cross_code_unit_false_match() {
        // UTF-16LE: U+0A41 then U+0100 -> bytes 41 0A 00 01.  A 1-byte scan
        // would see [0x0A, 0x00] straddling the two code units at offset 1 and
        // corrupt the stream; a 2-byte scan must leave the input untouched.
        let bytes = [0x41, 0x0A, 0x00, 0x01];
        let out = replace_u16_pairs(&bytes, [0x0A, 0x00], &[0x0D, 0x00, 0x0A, 0x00]);
        assert_eq!(out, bytes);
    }

    #[test]
    fn replace_u16_pairs_forwards_odd_trailing_byte() {
        // Malformed odd-length input: the trailing byte is preserved.
        let bytes = [0x0A, 0x00, 0x42];
        let out = replace_u16_pairs(&bytes, [0x0A, 0x00], &[0x0D, 0x00, 0x0A, 0x00]);
        assert_eq!(out, [0x0D, 0x00, 0x0A, 0x00, 0x42]);
    }

    #[test]
    fn apply_line_ending_to_all_utf16le_preserves_cross_unit_bytes() {
        // U+0A41, U+0100, then an actual LF unit (U+000A).  Converting to CRLF
        // must rewrite only the real line ending and leave the first two units
        // intact (no spurious match at the odd byte boundary).
        let bytes = vec![0x41, 0x0A, 0x00, 0x01, 0x0A, 0x00];
        let out = apply_line_ending_to_all(bytes, encoding_rs::UTF_16LE, LineEnding::CrLf);
        assert_eq!(out, [0x41, 0x0A, 0x00, 0x01, 0x0D, 0x00, 0x0A, 0x00]);
    }
}
