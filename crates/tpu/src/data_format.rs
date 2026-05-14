// Copyright (c) 2026, Michael Grier

use clap::ValueEnum;

/// The encoding format of the `DATA` positional argument supplied to
/// `write --binary --data-format`.
///
/// Changing any variant's wire representation (e.g. the clap string shown in
/// help) is a breaking change to the command-line interface.
#[derive(Clone, Debug, ValueEnum)]
pub enum DataFormat {
    /// Hexadecimal byte string.
    ///
    /// Uppercase or lowercase hex digits.  Bytes may optionally be separated
    /// by `-` (e.g. `4D-5A-00-00` or `4D5A0000`).  An odd number of hex
    /// digits after stripping separators is an error.
    Hex,

    /// Standard base-64 (RFC 4648) with required `=` padding.
    ///
    /// Surrounding ASCII whitespace is stripped before decoding.
    Base64,

    /// tpu escape codec (see `readex`).
    ///
    /// Printable ASCII passes through unchanged.  Recognised escape sequences:
    /// `\\`, `\0`, `\t`, `\n`, `\r`, `\xHH`, `\uXXXX`, `\UXXXXXXXX`.
    Encoded,
}

/// Decode `data` according to `format`.
///
/// Returns an explanatory error string (not an `std::error::Error` object)
/// suitable for displaying to the user as `tpu: <msg>`.
pub fn decode(format: &DataFormat, data: &str) -> Result<Vec<u8>, String> {
    match format {
        DataFormat::Hex => decode_hex(data),
        DataFormat::Base64 => decode_base64(data),
        DataFormat::Encoded => decode_encoded(data),
    }
}

// ── Hex ──────────────────────────────────────────────────────────────────────

fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    // Strip optional '-' byte separators, then validate and decode.
    let clean: String = s.chars().filter(|&c| c != '-').collect();
    if !clean.len().is_multiple_of(2) {
        return Err(format!(
            "hex data has an odd number of digits ({} digit{} after stripping dashes)",
            clean.len(),
            if clean.len() == 1 { "" } else { "s" },
        ));
    }
    let mut bytes = Vec::with_capacity(clean.len() / 2);
    for (i, chunk) in clean.as_bytes().chunks(2).enumerate() {
        let hi = hex_digit(chunk[0])
            .map_err(|c| format!("invalid hex digit {c:?} at position {}", i * 2))?;
        let lo = hex_digit(chunk[1])
            .map_err(|c| format!("invalid hex digit {c:?} at position {}", i * 2 + 1))?;
        bytes.push((hi << 4) | lo);
    }
    Ok(bytes)
}

fn hex_digit(b: u8) -> Result<u8, char> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(b as char),
    }
}

// ── Base64 ───────────────────────────────────────────────────────────────────

fn decode_base64(s: &str) -> Result<Vec<u8>, String> {
    // Strip surrounding ASCII whitespace (newlines, spaces, etc.).
    let stripped: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if stripped.is_empty() {
        return Ok(Vec::new());
    }
    if !stripped.len().is_multiple_of(4) {
        return Err(format!(
            "base64 input length ({} chars after stripping whitespace) is not a multiple of 4; \
             padding may be missing",
            stripped.len(),
        ));
    }
    let mut out = Vec::with_capacity(stripped.len() * 3 / 4);
    for (group, chunk) in stripped.chunks(4).enumerate() {
        let a = b64_val(chunk[0], group * 4)?;
        let b = b64_val(chunk[1], group * 4 + 1)?;
        // Third character: may be '=' padding (group produces only 1 output byte).
        if chunk[2] == b'=' {
            if chunk[3] != b'=' {
                return Err(format!(
                    "invalid base64 padding at position {}; expected '=' after '='",
                    group * 4 + 3
                ));
            }
            out.push((a << 2) | (b >> 4));
            break;
        }
        let c = b64_val(chunk[2], group * 4 + 2)?;
        out.push((a << 2) | (b >> 4));
        out.push((b << 4) | (c >> 2));
        // Fourth character: may be '=' padding (group produces only 2 output bytes).
        if chunk[3] == b'=' {
            break;
        }
        let d = b64_val(chunk[3], group * 4 + 3)?;
        out.push((c << 6) | d);
    }
    Ok(out)
}

fn b64_val(b: u8, pos: usize) -> Result<u8, String> {
    match b {
        b'A'..=b'Z' => Ok(b - b'A'),
        b'a'..=b'z' => Ok(b - b'a' + 26),
        b'0'..=b'9' => Ok(b - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        b'=' => Err(format!("unexpected '=' padding at position {pos}")),
        _ => Err(format!(
            "invalid base64 character {:?} at position {pos}",
            b as char
        )),
    }
}

// ── Encoded ──────────────────────────────────────────────────────────────────

fn decode_encoded(s: &str) -> Result<Vec<u8>, String> {
    crate::escape::decode_bytes(s).map_err(|e| e.to_string())
}

/// Encode `data` according to `format`, returning a `String`.
///
/// The output is always valid ASCII.  The result round-trips back to the
/// original bytes when passed to [`decode`] with the same format.
pub fn encode(format: &DataFormat, data: &[u8]) -> String {
    match format {
        DataFormat::Hex => encode_hex(data),
        DataFormat::Base64 => encode_base64_pem(data),
        DataFormat::Encoded => encode_encoded(data),
    }
}

// ── Encoded encoder ───────────────────────────────────────────────────────────

/// Encodes `data` in the tpu `encoded` codec.
///
/// - Printable ASCII bytes in the range `0x20`–`0x7E`, **excluding** `\`
///   (`0x5C`), are emitted unchanged.
/// - `\` (backslash) is emitted as `\\`.
/// - All other bytes are emitted as `\xHH` where `HH` is the **uppercase**
///   two-digit hexadecimal value of the byte.
///
/// The result is 7-bit clean and contains no literal newlines, making it safe
/// for shell variables and structured text consumers.  Decodes symmetrically
/// with [`decode`] using [`DataFormat::Encoded`].
///
/// Changing the output encoding for any byte value is a breaking change.
pub fn encode_encoded(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for &b in data {
        match b {
            b'\\' => out.push_str("\\\\"),
            // Printable ASCII excluding backslash: emit as-is.
            0x20..=0x7E => out.push(b as char),
            // Everything else: \xHH uppercase.
            _ => {
                out.push('\\');
                out.push('x');
                out.push(upper_nibble(b >> 4));
                out.push(upper_nibble(b & 0x0F));
            }
        }
    }
    out
}

#[inline]
fn upper_nibble(n: u8) -> char {
    if n < 10 {
        (b'0' + n) as char
    } else {
        (b'A' + n - 10) as char
    }
}

// ── Hex encoder ───────────────────────────────────────────────────────────────

/// Encodes `data` as `UU-UU-UU-...` where each `UU` is an uppercase
/// two-hex-digit pair.  Bytes are separated by `-`; no trailing `-` is emitted.
/// Empty input yields an empty string.
///
/// Decodes symmetrically with [`decode`] using [`DataFormat::Hex`].
///
/// Changing the separator or case is a breaking change.
pub fn encode_hex(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(data.len() * 3 - 1);
    for (i, &b) in data.iter().enumerate() {
        if i > 0 {
            out.push('-');
        }
        out.push(upper_nibble(b >> 4));
        out.push(upper_nibble(b & 0x0F));
    }
    out
}

// ── Base64 encoder ────────────────────────────────────────────────────────────

/// Encodes `data` as standard RFC 4648 Base64 (with `=` padding).
///
/// Used when writing binary payload into a JSON `data` message so that
/// arbitrary bytes can be embedded in the JSON string field without escaping
/// concerns.  Decodes symmetrically with [`decode`] using [`DataFormat::Base64`].
pub fn encode_base64(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let combined = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((combined >> 18) & 0x3F) as usize]);
        out.push(ALPHABET[((combined >> 12) & 0x3F) as usize]);
        out.push(if chunk.len() >= 2 {
            ALPHABET[((combined >> 6) & 0x3F) as usize]
        } else {
            b'='
        });
        out.push(if chunk.len() == 3 {
            ALPHABET[(combined & 0x3F) as usize]
        } else {
            b'='
        });
    }
    // SAFETY: output is always valid ASCII.
    String::from_utf8(out).unwrap()
}

// ── Base64 PEM encoder ──────────────────────────────────────────────────────

/// Encodes `data` as PEM-body Base64: standard RFC 4648 with `=` padding,
/// line-wrapped at 64 characters, each line terminated with `\r\n`.
///
/// Suitable for use with `--output-format base64` on binary reads.
/// Decodes symmetrically via [`decode`] using [`DataFormat::Base64`] because
/// `decode_base64` strips all ASCII whitespace (including `\r` and `\n`)
/// before processing.
pub fn encode_base64_pem(data: &[u8]) -> String {
    let flat = encode_base64(data);
    if flat.is_empty() {
        return flat;
    }
    let mut out = String::with_capacity(flat.len() + (flat.len() / 64) * 2 + 2);
    let mut pos = 0;
    while pos < flat.len() {
        let end = (pos + 64).min(flat.len());
        out.push_str(&flat[pos..end]);
        out.push_str("\r\n");
        pos = end;
    }
    out
}

// ── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Hex ──────────────────────────────────────────────────────────────────

    #[test]
    fn hex_empty() {
        assert_eq!(decode_hex("").unwrap(), b"");
    }

    #[test]
    fn hex_single_byte_lowercase() {
        assert_eq!(decode_hex("4d").unwrap(), &[0x4D]);
    }

    #[test]
    fn hex_single_byte_uppercase() {
        assert_eq!(decode_hex("4D").unwrap(), &[0x4D]);
    }

    #[test]
    fn hex_mz_header_dashes() {
        assert_eq!(
            decode_hex("4D-5A-00-00").unwrap(),
            &[0x4D, 0x5A, 0x00, 0x00]
        );
    }

    #[test]
    fn hex_mz_header_no_dashes() {
        assert_eq!(decode_hex("4D5A0000").unwrap(), &[0x4D, 0x5A, 0x00, 0x00]);
    }

    #[test]
    fn hex_all_zeros() {
        assert_eq!(decode_hex("000000").unwrap(), &[0x00, 0x00, 0x00]);
    }

    #[test]
    fn hex_all_ff() {
        assert_eq!(decode_hex("FFFFFF").unwrap(), &[0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn hex_mixed_case() {
        assert_eq!(decode_hex("aAbBcCdD").unwrap(), &[0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn hex_long_dashed() {
        let s = "01-02-03-04-05-06-07-08";
        assert_eq!(decode_hex(s).unwrap(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn hex_alternating_zero_ff() {
        assert_eq!(
            decode_hex("00-FF-00-FF-00-FF").unwrap(),
            &[0x00, 0xFF, 0x00, 0xFF, 0x00, 0xFF]
        );
    }

    #[test]
    fn hex_err_odd_digits() {
        assert!(decode_hex("4D5").is_err());
    }

    #[test]
    fn hex_err_single_digit() {
        assert!(decode_hex("A").is_err());
    }

    #[test]
    fn hex_err_invalid_char_g() {
        assert!(decode_hex("GG").is_err());
    }

    #[test]
    fn hex_err_invalid_char_space() {
        // Spaces are not stripped; only '-' separators are.
        assert!(decode_hex("4D 5A").is_err());
    }

    #[test]
    fn hex_all_nibbles() {
        // 0x0F-0xF0-0xAB round-trips through lowercase and uppercase variants.
        assert_eq!(decode_hex("0ff0ab").unwrap(), &[0x0F, 0xF0, 0xAB]);
        assert_eq!(decode_hex("0FF0AB").unwrap(), &[0x0F, 0xF0, 0xAB]);
    }

    // ── Base64 ───────────────────────────────────────────────────────────────

    #[test]
    fn b64_empty() {
        assert_eq!(decode_base64("").unwrap(), b"");
    }

    #[test]
    fn b64_hello() {
        // "Hello" base64 = "SGVsbG8="
        assert_eq!(decode_base64("SGVsbG8=").unwrap(), b"Hello");
    }

    #[test]
    fn b64_hello_world() {
        // "Hello, World!" base64 = "SGVsbG8sIFdvcmxkIQ=="
        assert_eq!(
            decode_base64("SGVsbG8sIFdvcmxkIQ==").unwrap(),
            b"Hello, World!"
        );
    }

    #[test]
    fn b64_man() {
        // "Man" = "TWFu" (no padding needed for 3 bytes)
        assert_eq!(decode_base64("TWFu").unwrap(), b"Man");
    }

    #[test]
    fn b64_one_zero_byte() {
        // 1 zero byte → "AA=="
        assert_eq!(decode_base64("AA==").unwrap(), &[0]);
    }

    #[test]
    fn b64_two_zero_bytes() {
        // 2 zero bytes → "AAA="
        assert_eq!(decode_base64("AAA=").unwrap(), &[0, 0]);
    }

    #[test]
    fn b64_three_zero_bytes() {
        // 3 zero bytes → "AAAA"
        assert_eq!(decode_base64("AAAA").unwrap(), &[0, 0, 0]);
    }

    #[test]
    fn b64_mz_header_two_bytes() {
        // 0x4D 0x5A → "TVo="
        assert_eq!(decode_base64("TVo=").unwrap(), &[0x4D, 0x5A]);
    }

    #[test]
    fn b64_whitespace_stripped() {
        // Leading/trailing whitespace is stripped before decoding.
        assert_eq!(decode_base64("  SGVsbG8=  \n").unwrap(), b"Hello");
    }

    #[test]
    fn b64_4_chars_no_pad_decodes_3_bytes() {
        // "SGVs" decodes to "Hel" (3 bytes, no padding needed).
        assert_eq!(decode_base64("SGVs").unwrap(), b"Hel");
    }

    #[test]
    fn b64_err_wrong_length_not_mult4() {
        assert!(decode_base64("SGVsb").is_err()); // 5 chars is not a multiple of 4
    }

    #[test]
    fn b64_err_invalid_char() {
        assert!(decode_base64("SG!s").is_err());
    }

    #[test]
    fn b64_err_bad_padding_order() {
        // '=' at position 2 without '=' at position 3 is invalid.
        assert!(decode_base64("AA=A").is_err());
    }

    // ── Encoded ──────────────────────────────────────────────────────────────

    #[test]
    fn encoded_empty() {
        assert_eq!(decode_encoded("").unwrap(), b"");
    }

    #[test]
    fn encoded_plain_ascii() {
        assert_eq!(decode_encoded("hello").unwrap(), b"hello");
    }

    #[test]
    fn encoded_hex_escape() {
        assert_eq!(decode_encoded("\\x4D\\x5A").unwrap(), &[0x4D, 0x5A]);
    }

    #[test]
    fn encoded_newline_escape() {
        assert_eq!(decode_encoded("line1\\nline2").unwrap(), b"line1\nline2");
    }

    #[test]
    fn encoded_tab_escape() {
        assert_eq!(decode_encoded("a\\tb").unwrap(), b"a\tb");
    }

    #[test]
    fn encoded_carriage_return_escape() {
        assert_eq!(decode_encoded("a\\rb").unwrap(), b"a\rb");
    }

    #[test]
    fn encoded_backslash_escape() {
        assert_eq!(decode_encoded("a\\\\b").unwrap(), b"a\\b");
    }

    #[test]
    fn encoded_null_escape() {
        assert_eq!(decode_encoded("\\0").unwrap(), &[0x00]);
    }

    #[test]
    fn encoded_unicode_4digit() {
        // \u0041 = 'A' in UTF-8
        assert_eq!(decode_encoded("\\u0041").unwrap(), b"A");
    }

    #[test]
    fn encoded_mixed() {
        assert_eq!(decode_encoded("Hello\\nWorld").unwrap(), b"Hello\nWorld");
    }

    #[test]
    fn encoded_err_bad_escape() {
        assert!(decode_encoded("\\q").is_err());
    }

    #[test]
    fn encoded_err_incomplete_hex_escape() {
        assert!(decode_encoded("\\x4").is_err());
    }

    // ── encode_encoded ────────────────────────────────────────────────────────

    #[test]
    fn encode_encoded_empty() {
        assert_eq!(encode_encoded(b""), "");
    }

    #[test]
    fn encode_encoded_plain_ascii() {
        assert_eq!(encode_encoded(b"hello"), "hello");
    }

    #[test]
    fn encode_encoded_backslash() {
        assert_eq!(encode_encoded(b"\\"), "\\\\");
    }

    #[test]
    fn encode_encoded_nul() {
        assert_eq!(encode_encoded(&[0x00]), "\\x00");
    }

    #[test]
    fn encode_encoded_tab() {
        assert_eq!(encode_encoded(b"\t"), "\\x09");
    }

    #[test]
    fn encode_encoded_newline() {
        assert_eq!(encode_encoded(b"\n"), "\\x0A");
    }

    #[test]
    fn encode_encoded_cr() {
        assert_eq!(encode_encoded(b"\r"), "\\x0D");
    }

    #[test]
    fn encode_encoded_del() {
        assert_eq!(encode_encoded(&[0x7F]), "\\x7F");
    }

    #[test]
    fn encode_encoded_high_byte() {
        assert_eq!(encode_encoded(&[0xFF]), "\\xFF");
    }

    #[test]
    fn encode_encoded_mz_header() {
        // 0x4D = 'M', 0x5A = 'Z' — both printable ASCII.
        assert_eq!(encode_encoded(&[0x4D, 0x5A, 0x00, 0x00]), "MZ\\x00\\x00");
    }

    #[test]
    fn encode_encoded_space_and_tilde() {
        // 0x20 (space) and 0x7E ('~') are boundary printable ASCII chars.
        assert_eq!(encode_encoded(&[0x20, 0x7E]), " ~");
    }

    #[test]
    fn encode_encoded_printable_range_boundaries() {
        // 0x1F (below printable) and 0x7F (DEL, above printable) are escaped.
        assert_eq!(encode_encoded(&[0x1F, 0x7F]), "\\x1F\\x7F");
    }

    #[test]
    fn encode_encoded_all_printable_ascii_no_backslash() {
        let printable: Vec<u8> = (0x20u8..=0x7Eu8).filter(|&b| b != b'\\').collect();
        let encoded = encode_encoded(&printable);
        let expected: String = printable.iter().map(|&b| b as char).collect();
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_encoded_uppercase_hex_digits() {
        assert_eq!(encode_encoded(&[0xAB, 0xCD, 0xEF]), "\\xAB\\xCD\\xEF");
    }

    #[test]
    fn encode_encoded_roundtrip_all_bytes() {
        let all: Vec<u8> = (0u8..=255u8).collect();
        let encoded = encode_encoded(&all);
        let decoded = decode_encoded(&encoded).expect("round-trip must decode");
        assert_eq!(decoded, all);
    }

    #[test]
    fn encode_encoded_roundtrip_backslash_only() {
        let input = b"a\\b\\c";
        let encoded = encode_encoded(input);
        let decoded = decode_encoded(&encoded).expect("round-trip must decode");
        assert_eq!(decoded, input);
    }

    // ── encode_hex ────────────────────────────────────────────────────────────

    #[test]
    fn encode_hex_empty() {
        assert_eq!(encode_hex(b""), "");
    }

    #[test]
    fn encode_hex_single_byte_low() {
        assert_eq!(encode_hex(&[0x00]), "00");
    }

    #[test]
    fn encode_hex_single_byte_high() {
        assert_eq!(encode_hex(&[0xFF]), "FF");
    }

    #[test]
    fn encode_hex_mz_header() {
        assert_eq!(encode_hex(&[0x4D, 0x5A, 0x00, 0x00]), "4D-5A-00-00");
    }

    #[test]
    fn encode_hex_uppercase() {
        assert_eq!(encode_hex(&[0xAB, 0xCD, 0xEF]), "AB-CD-EF");
    }

    #[test]
    fn encode_hex_no_trailing_dash() {
        let s = encode_hex(&[0x01, 0x02]);
        assert!(!s.ends_with('-'));
    }

    #[test]
    fn encode_hex_roundtrip_all_bytes() {
        let all: Vec<u8> = (0u8..=255u8).collect();
        let encoded = encode_hex(&all);
        let decoded = decode_hex(&encoded).expect("round-trip must decode");
        assert_eq!(decoded, all);
    }

    #[test]
    fn encode_hex_roundtrip_mz() {
        let input = &[0x4D, 0x5A, 0x00, 0x00u8];
        assert_eq!(decode_hex(&encode_hex(input)).unwrap(), input);
    }

    #[test]
    fn encode_hex_10_bytes_has_9_dashes() {
        let data: Vec<u8> = (0..10).collect();
        let s = encode_hex(&data);
        assert_eq!(s.chars().filter(|&c| c == '-').count(), 9);
    }

    // ── encode_base64_pem ─────────────────────────────────────────────────────

    #[test]
    fn encode_base64_pem_empty() {
        assert_eq!(encode_base64_pem(b""), "");
    }

    #[test]
    fn encode_base64_pem_short_no_wrap() {
        // "Man" encodes to "TWFu" — 4 chars, fits in one line.
        let s = encode_base64_pem(b"Man");
        assert_eq!(s, "TWFu\r\n");
    }

    #[test]
    fn encode_base64_pem_hello() {
        // "Hello" base64 = "SGVsbG8=" — fits in one line.
        let s = encode_base64_pem(b"Hello");
        assert_eq!(s, "SGVsbG8=\r\n");
    }

    #[test]
    fn encode_base64_pem_exact_48_bytes_is_one_line() {
        // 48 bytes → 64 base64 chars (exactly one PEM line, no split).
        let data = vec![0u8; 48];
        let s = encode_base64_pem(&data);
        // 64 A's + "=" padding already handled — all zeros → "AAAA...".
        let lines: Vec<&str> = s.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 64);
    }

    #[test]
    fn encode_base64_pem_49_bytes_wraps_to_two_lines() {
        // 49 bytes → 68 base64 chars → first line 64, second line 4.
        let data = vec![0u8; 49];
        let s = encode_base64_pem(&data);
        let lines: Vec<&str> = s.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 64);
        assert_eq!(lines[1].len(), 4);
    }

    #[test]
    fn encode_base64_pem_all_lines_end_crlf() {
        // Every line must be terminated with \r\n.
        let data: Vec<u8> = (0u8..=255u8).collect();
        let s = encode_base64_pem(&data);
        for ch in s.split('\n') {
            if !ch.is_empty() {
                assert!(ch.ends_with('\r'), "line must end with \\r before \\n");
            }
        }
    }

    #[test]
    fn encode_base64_pem_line_length_at_most_64() {
        let data: Vec<u8> = (0u8..=255u8).collect();
        let s = encode_base64_pem(&data);
        for line in s.split("\r\n").filter(|l| !l.is_empty()) {
            assert!(line.len() <= 64, "line too long: {}", line.len());
        }
    }

    #[test]
    fn encode_base64_pem_roundtrip_all_bytes() {
        let all: Vec<u8> = (0u8..=255u8).collect();
        let encoded = encode_base64_pem(&all);
        let decoded = decode_base64(&encoded).expect("round-trip must decode");
        assert_eq!(decoded, all);
    }

    #[test]
    fn encode_base64_pem_roundtrip_mz_header() {
        let input = &[0x4D, 0x5A, 0x00, 0x00u8];
        let encoded = encode_base64_pem(input);
        let decoded = decode_base64(&encoded).expect("round-trip must decode");
        assert_eq!(decoded, input);
    }

    #[test]
    fn encode_base64_pem_96_bytes_exactly_two_full_lines() {
        // 96 bytes → 128 base64 chars → 2 lines of 64 each.
        let data = vec![0xFFu8; 96];
        let s = encode_base64_pem(&data);
        let lines: Vec<&str> = s.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.len() == 64));
    }

    #[test]
    fn encode_base64_pem_single_byte() {
        // 1 byte encodes to 4 base64 chars with two padding '='.
        let s = encode_base64_pem(&[0x00]);
        assert_eq!(s, "AA==\r\n");
    }

    #[test]
    fn encode_base64_pem_two_bytes() {
        // 2 bytes → 4 chars with one padding '='.
        let s = encode_base64_pem(&[0x00, 0x00]);
        assert_eq!(s, "AAA=\r\n");
    }

    #[test]
    fn encode_base64_pem_large_roundtrip() {
        // 1000-byte payload to exercise multi-line wrap thoroughly.
        let data: Vec<u8> = (0u8..=255u8).cycle().take(1000).collect();
        let encoded = encode_base64_pem(&data);
        let decoded = decode_base64(&encoded).expect("large round-trip must decode");
        assert_eq!(decoded, data);
    }

    // ── decode_base64 PEM-style input (TPU-15.5 verification) ────────────────

    #[test]
    fn decode_base64_accepts_crlf_line_wrapped_input() {
        // decode_base64 uses is_ascii_whitespace() which covers \r (0x0D) and
        // \n (0x0A), so PEM-style CRLF-terminated lines decode transparently.
        let pem = "SGVs\r\nbG8=\r\n";
        assert_eq!(decode_base64(pem).unwrap(), b"Hello");
    }

    #[test]
    fn decode_base64_accepts_lf_only_line_wrapped_input() {
        let lf = "SGVs\nbG8=\n";
        assert_eq!(decode_base64(lf).unwrap(), b"Hello");
    }

    #[test]
    fn decode_base64_accepts_64char_lines_crlf() {
        // Build a PEM body manually: 64 chars per line, CRLF terminated.
        // 48 zero bytes -> 64 base64 'A' chars, "AAAA...AAAA==".
        // Actually 48 zero bytes -> exactly 64 'A' chars (no padding).
        let data = vec![0u8; 48];
        let flat = encode_base64(&data);
        assert_eq!(flat.len(), 64);
        let pem = format!("{}\r\n", flat);
        let decoded = decode_base64(&pem).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn decode_base64_accepts_multiline_crlf_all_bytes() {
        // Full roundtrip: encode with PEM wrapper, decode, must equal original.
        let all: Vec<u8> = (0u8..=255u8).collect();
        let pem = encode_base64_pem(&all);
        // Confirm it really has \r\n in it.
        assert!(pem.contains("\r\n"));
        let decoded = decode_base64(&pem).unwrap();
        assert_eq!(decoded, all);
    }

    #[test]
    fn decode_base64_pem_empty_lines_ignored() {
        // Extra blank CRLF lines (as may appear around PEM headers) are stripped.
        let pem = "\r\nSGVsbG8=\r\n\r\n";
        assert_eq!(decode_base64(pem).unwrap(), b"Hello");
    }
}
