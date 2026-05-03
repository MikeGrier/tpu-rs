// Copyright (c) 2026, Michael Grier

//! Unicode escape codec for the `readex` subcommand and `--data-format=encoded`.
//!
//! Encodes any Unicode string to a 7-bit clean ASCII representation and
//! decodes it back, with round-trip correctness for all valid Unicode scalar
//! values (U+0000–U+10FFFF, excluding surrogates).
//!
//! A parallel byte-level codec (`encode_bytes` / `decode_bytes`) operates on
//! raw `&[u8]` slices for binary mode.  Non-printable bytes are represented
//! as `\xHH` (lowercase hex).  The byte codec is a superset of the text
//! codec: named escapes are shared, and `\uXXXX` / `\UXXXXXXXX` sequences are
//! accepted by `decode_bytes` (encoding the code point as UTF-8 bytes).
//!
//! ## Escape table
//!
//! | Sequence      | Meaning                                         |
//! |---------------|-------------------------------------------------|
//! | `\\`          | Literal backslash (U+005C / 0x5C)              |
//! | `\0`          | NUL (U+0000 / 0x00)                            |
//! | `\t`          | Horizontal tab (U+0009 / 0x09)                 |
//! | `\n`          | Line feed (U+000A / 0x0A)                      |
//! | `\r`          | Carriage return (U+000D / 0x0D)                |
//! | `\uXXXX`      | BMP scalar value — exactly 4 hex digits         |
//! | `\UXXXXXXXX`  | Supplementary scalar — exactly 8 hex digits     |
//! | `\xHH`        | Single raw byte — exactly 2 hex digits (byte codec only) |
//!
//! Printable ASCII (U+0020–U+007E, except `\`) passes through unescaped.
//!
//! Changing the encoding of any sequence above is a breaking change for both
//! readex consumers and encoded-format writers.

use std::fmt;

// ──────────────────────────────────────────────────────────────────────────────
// Error type
// ──────────────────────────────────────────────────────────────────────────────

/// Errors that can occur while decoding an escape sequence.
#[derive(Debug, PartialEq)]
pub enum DecodeError {
    /// The input ended while an escape sequence was being parsed.
    UnexpectedEnd {
        /// Byte offset of the `\` that started the incomplete sequence.
        at: usize,
    },
    /// An unrecognised character followed a backslash.
    UnknownEscape {
        /// Byte offset of the `\`.
        at: usize,
        /// The character that followed `\`.
        ch: char,
    },
    /// A non-hex character appeared inside a `\uXXXX` or `\UXXXXXXXX` run.
    InvalidHex {
        /// Byte offset of the `\` that started the sequence.
        at: usize,
        /// The offending character.
        ch: char,
    },
    /// A `\uXXXX` or `\UXXXXXXXX` sequence decoded to a surrogate code point
    /// (U+D800–U+DFFF) or a value above U+10FFFF.
    InvalidScalar {
        /// Byte offset of the `\` that started the sequence.
        at: usize,
        /// The decoded numeric value that is not a valid Unicode scalar.
        value: u32,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnexpectedEnd { at } => {
                write!(f, "escape sequence truncated at offset {at}")
            }
            DecodeError::UnknownEscape { at, ch } => {
                write!(f, "unknown escape '\\{ch}' at offset {at}")
            }
            DecodeError::InvalidHex { at, ch } => {
                write!(f, "non-hex digit {ch:?} in escape sequence at offset {at}")
            }
            DecodeError::InvalidScalar { at, value } => {
                write!(
                    f,
                    "U+{value:08X} is not a valid Unicode scalar value (offset {at})"
                )
            }
        }
    }
}

impl std::error::Error for DecodeError {}

// ──────────────────────────────────────────────────────────────────────────────
// Encoder
// ──────────────────────────────────────────────────────────────────────────────

/// Encode a Unicode string as a 7-bit clean ASCII escaped representation.
///
/// Every character outside the printable ASCII range (U+0020–U+007E, except
/// backslash) is replaced by one of the named or `\uXXXX` / `\UXXXXXXXX`
/// escape sequences in the module-level table.  The result contains no bytes
/// above 127 and no literal control characters.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\x00' => out.push_str("\\0"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            // Printable ASCII except backslash — pass through unescaped.
            '\x20'..='\x7E' => out.push(ch),
            // Other BMP characters (control chars, non-ASCII) → \uXXXX.
            c if (c as u32) <= 0xFFFF => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            // Supplementary plane characters → \UXXXXXXXX.
            c => {
                out.push_str(&format!("\\U{:08X}", c as u32));
            }
        }
    }
    out
}

// ──────────────────────────────────────────────────────────────────────────────
// Decoder
// ──────────────────────────────────────────────────────────────────────────────

/// Decode a 7-bit clean ASCII escaped string back to a Unicode string.
///
/// Returns `Ok(String)` on success or a [`DecodeError`] on the first invalid
/// escape sequence encountered.
#[allow(dead_code)]
pub fn decode(s: &str) -> Result<String, DecodeError> {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'\\' {
            // Non-backslash byte — valid escaped input only contains printable
            // ASCII here, so casting to char is correct for the 0x00–0x7F range.
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }

        let backslash_at = i;
        i += 1; // Consume the `\`.

        if i >= bytes.len() {
            return Err(DecodeError::UnexpectedEnd { at: backslash_at });
        }

        match bytes[i] {
            b'\\' => {
                out.push('\\');
                i += 1;
            }
            b'0' => {
                out.push('\x00');
                i += 1;
            }
            b't' => {
                out.push('\t');
                i += 1;
            }
            b'n' => {
                out.push('\n');
                i += 1;
            }
            b'r' => {
                out.push('\r');
                i += 1;
            }
            b'u' => {
                i += 1; // Consume the `u`.
                let value = decode_hex_digits(bytes, backslash_at, &mut i, 4)?;
                let ch = char::from_u32(value).ok_or(DecodeError::InvalidScalar {
                    at: backslash_at,
                    value,
                })?;
                out.push(ch);
            }
            b'U' => {
                i += 1; // Consume the `U`.
                let value = decode_hex_digits(bytes, backslash_at, &mut i, 8)?;
                let ch = char::from_u32(value).ok_or(DecodeError::InvalidScalar {
                    at: backslash_at,
                    value,
                })?;
                out.push(ch);
            }
            b => {
                return Err(DecodeError::UnknownEscape {
                    at: backslash_at,
                    ch: b as char,
                });
            }
        }
    }

    Ok(out)
}

// ──────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Consume exactly `count` ASCII hex digits from `bytes` starting at `*pos`,
/// decode them as a big-endian `u32`, and advance `*pos` past the consumed
/// digits.  Returns [`DecodeError`] on a non-hex digit or premature end.
fn decode_hex_digits(
    bytes: &[u8],
    backslash_at: usize,
    pos: &mut usize,
    count: usize,
) -> Result<u32, DecodeError> {
    let mut value: u32 = 0;
    for _ in 0..count {
        if *pos >= bytes.len() {
            return Err(DecodeError::UnexpectedEnd { at: backslash_at });
        }
        let b = bytes[*pos];
        let digit = match b {
            b'0'..=b'9' => (b - b'0') as u32,
            b'a'..=b'f' => (b - b'a' + 10) as u32,
            b'A'..=b'F' => (b - b'A' + 10) as u32,
            _ => {
                return Err(DecodeError::InvalidHex {
                    at: backslash_at,
                    ch: b as char,
                });
            }
        };
        value = (value << 4) | digit;
        *pos += 1;
    }
    Ok(value)
}

// ──────────────────────────────────────────────────────────────────────────────
// Binary codec
// ──────────────────────────────────────────────────────────────────────────────

/// Encode a raw byte slice as a 7-bit clean ASCII escaped representation.
///
/// This is the byte-level analogue of [`encode`].  Bytes in the printable
/// ASCII range (0x20–0x7E, excluding `\`) are forwarded unchanged.  All
/// other bytes are escaped:
///
/// - 0x5C (`\`) → `\\`
/// - 0x00 (NUL) → `\0`
/// - 0x09 (tab) → `\t`
/// - 0x0A (LF)  → `\n`
/// - 0x0D (CR)  → `\r`
/// - everything else → `\xHH` (lowercase two-digit hex)
///
/// The result contains only printable ASCII and the above escape sequences.
pub fn encode_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\x00' => out.push_str("\\0"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            // Printable ASCII except backslash.
            0x20..=0x7E => out.push(b as char),
            // Everything else: two hex digits.
            _ => {
                out.push('\\');
                out.push('x');
                out.push(hex_nibble(b >> 4));
                out.push(hex_nibble(b & 0x0F));
            }
        }
    }
    out
}

/// Decode a 7-bit clean ASCII escaped string back to a raw byte vector.
///
/// This is the byte-level analogue of [`decode`].  All named escape sequences
/// from the text codec are supported, plus:
///
/// - `\xHH` — one raw byte whose hexadecimal value is `HH`.
/// - `\uXXXX` — the given code point encoded as UTF-8 bytes.
/// - `\UXXXXXXXX` — the given code point encoded as UTF-8 bytes.
///
/// Returns `Err` on an invalid escape sequence (same [`DecodeError`] variants
/// as [`decode`]).
pub fn decode_bytes(s: &str) -> Result<Vec<u8>, DecodeError> {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }

        let backslash_at = i;
        i += 1; // consume `\`

        if i >= bytes.len() {
            return Err(DecodeError::UnexpectedEnd { at: backslash_at });
        }

        match bytes[i] {
            b'\\' => {
                out.push(b'\\');
                i += 1;
            }
            b'0' => {
                out.push(b'\x00');
                i += 1;
            }
            b't' => {
                out.push(b'\t');
                i += 1;
            }
            b'n' => {
                out.push(b'\n');
                i += 1;
            }
            b'r' => {
                out.push(b'\r');
                i += 1;
            }
            b'x' => {
                i += 1; // consume `x`
                let value = decode_hex_digits(bytes, backslash_at, &mut i, 2)?;
                out.push(value as u8);
            }
            b'u' => {
                i += 1; // consume `u`
                let value = decode_hex_digits(bytes, backslash_at, &mut i, 4)?;
                let ch = char::from_u32(value).ok_or(DecodeError::InvalidScalar {
                    at: backslash_at,
                    value,
                })?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            b'U' => {
                i += 1; // consume `U`
                let value = decode_hex_digits(bytes, backslash_at, &mut i, 8)?;
                let ch = char::from_u32(value).ok_or(DecodeError::InvalidScalar {
                    at: backslash_at,
                    value,
                })?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            b => {
                return Err(DecodeError::UnknownEscape {
                    at: backslash_at,
                    ch: b as char,
                });
            }
        }
    }

    Ok(out)
}

/// Return the lowercase ASCII hex nibble for a 4-bit value (0–15).
#[inline]
fn hex_nibble(n: u8) -> char {
    if n < 10 {
        (b'0' + n) as char
    } else {
        (b'a' + n - 10) as char
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── encode: normal cases ────────────────────────────────────────────────

    #[test]
    fn encode_empty_string() {
        assert_eq!(encode(""), "");
    }

    #[test]
    fn encode_plain_ascii_passthrough() {
        assert_eq!(encode("hello world"), "hello world");
    }

    #[test]
    fn encode_all_printable_ascii_passthrough() {
        // All printable ASCII except backslash should pass through unchanged.
        let printable: String = (0x20u8..=0x7Eu8)
            .filter(|&b| b != b'\\')
            .map(|b| b as char)
            .collect();
        let encoded = encode(&printable);
        assert_eq!(encoded, printable);
    }

    #[test]
    fn encode_backslash_doubled() {
        assert_eq!(encode("a\\b"), "a\\\\b");
    }

    #[test]
    fn encode_nul_byte() {
        assert_eq!(encode("\x00"), "\\0");
    }

    #[test]
    fn encode_tab() {
        assert_eq!(encode("\t"), "\\t");
    }

    #[test]
    fn encode_line_feed() {
        assert_eq!(encode("\n"), "\\n");
    }

    #[test]
    fn encode_carriage_return() {
        assert_eq!(encode("\r"), "\\r");
    }

    #[test]
    fn encode_bmp_latin_extended() {
        // é = U+00E9
        assert_eq!(encode("é"), "\\u00E9");
    }

    #[test]
    fn encode_bmp_cjk() {
        // 日 = U+65E5
        assert_eq!(encode("日"), "\\u65E5");
    }

    #[test]
    fn encode_supplementary_emoji() {
        // 😀 = U+1F600
        assert_eq!(encode("😀"), "\\U0001F600");
    }

    #[test]
    fn encode_supplementary_high_plane() {
        // U+10FFFF is the maximum valid Unicode scalar.
        assert_eq!(encode("\u{10FFFF}"), "\\U0010FFFF");
    }

    #[test]
    fn encode_del_control_char() {
        // U+007F DEL is not printable ASCII — must be escaped.
        assert_eq!(encode("\x7F"), "\\u007F");
    }

    #[test]
    fn encode_crlf_sequence() {
        assert_eq!(encode("\r\n"), "\\r\\n");
    }

    #[test]
    fn encode_bmp_high_nonchar() {
        // U+FFFF is a BMP non-character.
        assert_eq!(encode("\u{FFFF}"), "\\uFFFF");
    }

    #[test]
    fn encode_mixed_content() {
        // Mix of passthrough ASCII, named escapes, and \uXXXX.
        let s = "line1\nline2\t\u{00E9}end";
        assert_eq!(encode(s), "line1\\nline2\\t\\u00E9end");
    }

    #[test]
    fn encode_space_is_passthrough() {
        assert_eq!(encode(" "), " ");
    }

    #[test]
    fn encode_control_char_unit_separator() {
        // U+001F (Unit Separator) is a non-named control char → \u001F.
        assert_eq!(encode("\x1F"), "\\u001F");
    }

    // ── decode: normal cases ────────────────────────────────────────────────

    #[test]
    fn decode_empty_string() {
        assert_eq!(decode("").unwrap(), "");
    }

    #[test]
    fn decode_plain_ascii_passthrough() {
        assert_eq!(decode("hello world").unwrap(), "hello world");
    }

    #[test]
    fn decode_backslash_escape() {
        assert_eq!(decode("\\\\").unwrap(), "\\");
    }

    #[test]
    fn decode_nul_escape() {
        assert_eq!(decode("\\0").unwrap(), "\x00");
    }

    #[test]
    fn decode_tab_escape() {
        assert_eq!(decode("\\t").unwrap(), "\t");
    }

    #[test]
    fn decode_lf_escape() {
        assert_eq!(decode("\\n").unwrap(), "\n");
    }

    #[test]
    fn decode_cr_escape() {
        assert_eq!(decode("\\r").unwrap(), "\r");
    }

    #[test]
    fn decode_u4_bmp() {
        assert_eq!(decode("\\u00E9").unwrap(), "é");
    }

    #[test]
    fn decode_u8_supplementary() {
        assert_eq!(decode("\\U0001F600").unwrap(), "😀");
    }

    #[test]
    fn decode_mixed_content() {
        assert_eq!(decode("a\\nb").unwrap(), "a\nb");
    }

    #[test]
    fn decode_u4_case_insensitive_lower() {
        assert_eq!(decode("\\u00e9").unwrap(), "é");
    }

    #[test]
    fn decode_u4_case_insensitive_upper() {
        assert_eq!(decode("\\u00E9").unwrap(), "é");
    }

    #[test]
    fn decode_u8_case_insensitive() {
        assert_eq!(decode("\\U0001f600").unwrap(), "😀");
    }

    // ── round-trip correctness ──────────────────────────────────────────────

    #[test]
    fn roundtrip_all_named_escapes() {
        let s = "\\\x00\t\n\r";
        assert_eq!(decode(&encode(s)).unwrap(), s);
    }

    #[test]
    fn roundtrip_bmp_non_ascii() {
        let s = "Héllo Wörld";
        assert_eq!(decode(&encode(s)).unwrap(), s);
    }

    #[test]
    fn roundtrip_supplementary_chars() {
        let s = "Hi 😀💯\u{10FFFF}";
        assert_eq!(decode(&encode(s)).unwrap(), s);
    }

    #[test]
    fn roundtrip_nul() {
        let s = "\x00";
        assert_eq!(decode(&encode(s)).unwrap(), s);
    }

    #[test]
    fn roundtrip_all_ascii_printable() {
        let s: String = (0x20u8..=0x7Eu8).map(|b| b as char).collect();
        assert_eq!(decode(&encode(&s)).unwrap(), s);
    }

    #[test]
    fn roundtrip_full_unicode_sample() {
        // A selection spanning ASCII, Latin, CJK, emoji, and supplementary.
        let s = "hello\tworld\n\u{00E9}\u{65E5}\u{1F600}\u{10FFFF}back\\slash";
        assert_eq!(decode(&encode(s)).unwrap(), s);
    }

    // ── decode: error cases ─────────────────────────────────────────────────

    #[test]
    fn decode_lone_backslash_at_end() {
        assert_eq!(decode("\\"), Err(DecodeError::UnexpectedEnd { at: 0 }));
    }

    #[test]
    fn decode_unknown_escape_x() {
        assert_eq!(
            decode("\\x41"),
            Err(DecodeError::UnknownEscape { at: 0, ch: 'x' })
        );
    }

    #[test]
    fn decode_unknown_escape_a() {
        assert_eq!(
            decode("\\a"),
            Err(DecodeError::UnknownEscape { at: 0, ch: 'a' })
        );
    }

    #[test]
    fn decode_invalid_hex_in_u4() {
        // 'Z' is not a hex digit.
        assert_eq!(
            decode("\\u00ZZ"),
            Err(DecodeError::InvalidHex { at: 0, ch: 'Z' })
        );
    }

    #[test]
    fn decode_truncated_u4_two_digits() {
        assert_eq!(decode("\\u00"), Err(DecodeError::UnexpectedEnd { at: 0 }));
    }

    #[test]
    fn decode_truncated_u4_zero_digits() {
        assert_eq!(decode("\\u"), Err(DecodeError::UnexpectedEnd { at: 0 }));
    }

    #[test]
    fn decode_truncated_u8_four_digits() {
        assert_eq!(decode("\\U0001"), Err(DecodeError::UnexpectedEnd { at: 0 }));
    }

    #[test]
    fn decode_surrogate_low_rejected() {
        // U+D800 is the start of the surrogate range — not a scalar value.
        assert_eq!(
            decode("\\uD800"),
            Err(DecodeError::InvalidScalar {
                at: 0,
                value: 0xD800
            })
        );
    }

    #[test]
    fn decode_surrogate_high_rejected() {
        // U+DFFF is the end of the surrogate range.
        assert_eq!(
            decode("\\uDFFF"),
            Err(DecodeError::InvalidScalar {
                at: 0,
                value: 0xDFFF
            })
        );
    }

    #[test]
    fn decode_above_unicode_max_rejected() {
        // U+110000 is one above the maximum Unicode code point.
        assert_eq!(
            decode("\\U00110000"),
            Err(DecodeError::InvalidScalar {
                at: 0,
                value: 0x0011_0000
            })
        );
    }

    #[test]
    fn decode_error_offset_nonzero() {
        // Error offset correctly reports position of the bad escape, not 0.
        assert_eq!(decode("abc\\"), Err(DecodeError::UnexpectedEnd { at: 3 }));
        assert_eq!(
            decode("abc\\x"),
            Err(DecodeError::UnknownEscape { at: 3, ch: 'x' })
        );
    }

    // ── encode_bytes: normal cases ────────────────────────────────────────────

    #[test]
    fn encode_bytes_empty() {
        assert_eq!(encode_bytes(b""), "");
    }

    #[test]
    fn encode_bytes_plain_ascii_passthrough() {
        assert_eq!(encode_bytes(b"hello world"), "hello world");
    }

    #[test]
    fn encode_bytes_all_printable_ascii_passthrough() {
        // All printable ASCII except backslash passes through unchanged.
        let printable: Vec<u8> = (0x20u8..=0x7Eu8).filter(|&b| b != b'\\').collect();
        let encoded = encode_bytes(&printable);
        let expected: String = printable.iter().map(|&b| b as char).collect();
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encode_bytes_backslash_doubled() {
        assert_eq!(encode_bytes(b"a\\b"), "a\\\\b");
    }

    #[test]
    fn encode_bytes_nul_byte() {
        assert_eq!(encode_bytes(b"\x00"), "\\0");
    }

    #[test]
    fn encode_bytes_tab() {
        assert_eq!(encode_bytes(b"\t"), "\\t");
    }

    #[test]
    fn encode_bytes_lf() {
        assert_eq!(encode_bytes(b"\n"), "\\n");
    }

    #[test]
    fn encode_bytes_cr() {
        assert_eq!(encode_bytes(b"\r"), "\\r");
    }

    #[test]
    fn encode_bytes_high_byte_ff() {
        assert_eq!(encode_bytes(b"\xFF"), "\\xff");
    }

    #[test]
    fn encode_bytes_arbitrary_byte_ab() {
        assert_eq!(encode_bytes(b"\xAB"), "\\xab");
    }

    #[test]
    fn encode_bytes_del_byte() {
        // 0x7F (DEL) is not printable ASCII — must be escaped.
        assert_eq!(encode_bytes(b"\x7F"), "\\x7f");
    }

    #[test]
    fn encode_bytes_byte_1f() {
        // 0x1F (Unit Separator) is a control byte — must be escaped.
        assert_eq!(encode_bytes(b"\x1F"), "\\x1f");
    }

    #[test]
    fn encode_bytes_mixed_content() {
        let input = b"\x00\tHello\n\xFF";
        assert_eq!(encode_bytes(input), "\\0\\tHello\\n\\xff");
    }

    #[test]
    fn encode_bytes_crlf_sequence() {
        assert_eq!(encode_bytes(b"\r\n"), "\\r\\n");
    }

    // ── decode_bytes: normal cases ────────────────────────────────────────────

    #[test]
    fn decode_bytes_empty() {
        assert_eq!(decode_bytes("").unwrap(), b"" as &[u8]);
    }

    #[test]
    fn decode_bytes_plain_ascii() {
        assert_eq!(decode_bytes("hello").unwrap(), b"hello" as &[u8]);
    }

    #[test]
    fn decode_bytes_backslash_escape() {
        assert_eq!(decode_bytes("\\\\").unwrap(), b"\\" as &[u8]);
    }

    #[test]
    fn decode_bytes_nul_escape() {
        assert_eq!(decode_bytes("\\0").unwrap(), b"\x00" as &[u8]);
    }

    #[test]
    fn decode_bytes_tab_escape() {
        assert_eq!(decode_bytes("\\t").unwrap(), b"\t" as &[u8]);
    }

    #[test]
    fn decode_bytes_lf_escape() {
        assert_eq!(decode_bytes("\\n").unwrap(), b"\n" as &[u8]);
    }

    #[test]
    fn decode_bytes_cr_escape() {
        assert_eq!(decode_bytes("\\r").unwrap(), b"\r" as &[u8]);
    }

    #[test]
    fn decode_bytes_hex_escape_ff() {
        assert_eq!(decode_bytes("\\xff").unwrap(), b"\xFF" as &[u8]);
    }

    #[test]
    fn decode_bytes_hex_escape_uppercase() {
        assert_eq!(decode_bytes("\\xAB").unwrap(), b"\xAB" as &[u8]);
    }

    #[test]
    fn decode_bytes_u4_yields_utf8_bytes() {
        // \u00E9 = é; as UTF-8: [0xC3, 0xA9]
        let result = decode_bytes("\\u00E9").unwrap();
        assert_eq!(result, "é".as_bytes());
    }

    #[test]
    fn decode_bytes_capital_u_yields_utf8_bytes() {
        // \U0001F600 = 😀; as UTF-8: [0xF0, 0x9F, 0x98, 0x80]
        let result = decode_bytes("\\U0001F600").unwrap();
        assert_eq!(result, "😀".as_bytes());
    }

    // ── decode_bytes: round-trip ──────────────────────────────────────────────

    #[test]
    fn roundtrip_bytes_all_256_values() {
        let all_bytes: Vec<u8> = (0u8..=255u8).collect();
        let encoded = encode_bytes(&all_bytes);
        let decoded = decode_bytes(&encoded).unwrap();
        assert_eq!(decoded, all_bytes);
    }

    #[test]
    fn roundtrip_bytes_printable_ascii() {
        let printable: Vec<u8> = (0x20u8..=0x7Eu8).filter(|&b| b != b'\\').collect();
        assert_eq!(decode_bytes(&encode_bytes(&printable)).unwrap(), printable);
    }

    #[test]
    fn roundtrip_bytes_all_named_escapes() {
        let input: &[u8] = b"\\\x00\t\n\r";
        assert_eq!(decode_bytes(&encode_bytes(input)).unwrap(), input);
    }

    // ── decode_bytes: error cases ─────────────────────────────────────────────

    #[test]
    fn decode_bytes_truncated_x_escape_one_digit() {
        // \x with only one hex digit → UnexpectedEnd.
        assert_eq!(
            decode_bytes("\\x4"),
            Err(DecodeError::UnexpectedEnd { at: 0 })
        );
    }

    #[test]
    fn decode_bytes_truncated_x_escape_no_digits() {
        // \x with no digits → UnexpectedEnd.
        assert_eq!(
            decode_bytes("\\x"),
            Err(DecodeError::UnexpectedEnd { at: 0 })
        );
    }

    #[test]
    fn decode_bytes_bad_hex_in_x_escape() {
        assert_eq!(
            decode_bytes("\\xGG"),
            Err(DecodeError::InvalidHex { at: 0, ch: 'G' })
        );
    }

    #[test]
    fn decode_bytes_lone_backslash() {
        assert_eq!(
            decode_bytes("\\"),
            Err(DecodeError::UnexpectedEnd { at: 0 })
        );
    }

    #[test]
    fn decode_bytes_unknown_escape_z() {
        assert_eq!(
            decode_bytes("\\z"),
            Err(DecodeError::UnknownEscape { at: 0, ch: 'z' })
        );
    }

    #[test]
    fn decode_bytes_error_offset_nonzero() {
        assert_eq!(
            decode_bytes("ok\\"),
            Err(DecodeError::UnexpectedEnd { at: 2 })
        );
        assert_eq!(
            decode_bytes("ok\\z"),
            Err(DecodeError::UnknownEscape { at: 2, ch: 'z' })
        );
    }
}
