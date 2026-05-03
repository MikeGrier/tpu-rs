// Copyright (c) 2026, Michael Grier

//! Curated mojibake byte sequences used by `tpu`'s own unit and
//! integration tests.
//!
//! ## Why this module exists
//!
//! The mojibake guard, the read-time advisory, and `tpu doctor` all
//! need to be exercised against text whose *bytes* spell out canonical
//! mojibake fingerprints (`Ã©`, `â€"`, `â"€`, `Â<NBSP>`).  Embedding
//! those bytes literally in `.rs` source files trips third-party
//! tooling (editors, code review systems, CI scanners) that does not
//! recognise our `encoding-check: allow-mojibake` opt-out marker.
//!
//! Every mojibake byte sequence consumed by the test suite lives here
//! as a base-64 constant (`*_B64`) and is decoded on demand by an
//! accompanying accessor (`cafe_mojibake()`, `em_dash_mojibake()`, …).
//! The `.rs` files in `crates/tpu/tests/` and the unit-test modules in
//! `crates/tpu/src/{mojibake,cmd/doctor}.rs` reference the accessors
//! and never spell the bytes out themselves.  The base-64 form is
//! pure ASCII, so no allow-marker is needed and no third-party tool
//! flags it.
//!
//! ## Stability
//!
//! These fixtures are *test infrastructure*, not API.  They are public
//! only because integration tests live in a separate crate and need
//! cross-crate access; downstream consumers should treat them as
//! unstable.  Each constant's docstring spells out the exact UTF-8
//! sequence it decodes to, so a reviewer never has to reach for a
//! base-64 decoder.

/// Decode a base-64 string (no whitespace, no padding ambiguity) into
/// a byte vector.  Panics on invalid input — only fed compile-time
/// literals defined in this file.
fn b64(s: &str) -> Vec<u8> {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4 + 2);
    let val = |b: u8| -> u8 {
        TABLE
            .iter()
            .position(|&x| x == b)
            .map(|v| v as u8)
            .unwrap_or(0)
    };
    for chunk in clean.chunks(4) {
        let b = [
            val(chunk[0]),
            val(chunk[1]),
            if chunk.len() > 2 { val(chunk[2]) } else { 0 },
            if chunk.len() > 3 { val(chunk[3]) } else { 0 },
        ];
        out.push((b[0] << 2) | (b[1] >> 4));
        if chunk.len() > 2 && *chunk.get(2).unwrap_or(&b'=') != b'=' {
            out.push((b[1] << 4) | (b[2] >> 2));
        }
        if chunk.len() > 3 && *chunk.get(3).unwrap_or(&b'=') != b'=' {
            out.push((b[2] << 6) | b[3]);
        }
    }
    out
}

/// Decode `s` and convert to a `String`.  Panics if the result is not
/// valid UTF-8 (every fixture in this module is, by construction).
fn s(b: &str) -> String {
    String::from_utf8(b64(b)).expect("test_fixtures: invalid UTF-8 after base64 decode")
}

// ── Atoms ─────────────────────────────────────────────────────────────────────
//
// Each atom is one canonical fingerprint, in isolation.  Atoms compose
// freely (string concatenation) into longer fixtures.

/// `cafÃ©` — the canonical Latin-1 fingerprint (U+00C3 U+00A9).  This
/// is what `café` looks like after a UTF-8 → cp1252 → UTF-8 round-trip.
/// Base-64 of `cafÃ©` (UTF-8: `63 61 66 C3 83 C2 A9`).
pub const CAFE_B64: &str = "Y2Fmw4PCqQ==";

/// `Ã©` — the bare Latin-1 fingerprint (U+00C3 U+00A9), no surrounding
/// text.  Two-character mojibake fragment.
/// Base-64 of `Ã©` (UTF-8: `C3 83 C2 A9`).
pub const LATIN1_FRAGMENT_B64: &str = "w4PCqQ==";

/// `Ã¨` — Latin-1 fingerprint for `è` (U+00E8) misread through cp1252.
/// Base-64 of `Ã¨` (UTF-8: `C3 83 C2 A8`).
pub const E_GRAVE_B64: &str = "w4PCqA==";

/// `Ã¯` — Latin-1 fingerprint for `ï` (U+00EF) misread through cp1252.
/// Base-64 of `naÃ¯ve` (UTF-8: `6E 61 C3 83 C2 AF 76 65`).
pub const NAIVE_B64: &str = "bmHDg8KvdmU=";

/// `â€"` — punctuation (em-dash) fingerprint:
/// U+00E2 U+20AC U+201D.  This is what `—` (U+2014) looks like after
/// a UTF-8 → cp1252 → UTF-8 round-trip.
/// Base-64 of `â€\u{201D}` (UTF-8: `C3 A2 E2 82 AC E2 80 9D`).
pub const EM_DASH_B64: &str = "w6LigqzigJ0=";

/// `â"€` — box-drawing fingerprint:
/// U+00E2 U+201D U+20AC.  This is what `─` (U+2500) looks like after
/// a UTF-8 → cp1252 → UTF-8 round-trip.
/// Base-64 of `â\u{201D}\u{20AC}` (UTF-8: `C3 A2 E2 80 9D E2 82 AC`).
pub const BOX_DRAWING_B64: &str = "w6LigJ3igqw=";

/// `Â<NBSP>` — non-breaking-space fingerprint:
/// U+00C2 U+00A0.
/// Base-64 of `Â\u{00A0}` (UTF-8: `C3 82 C2 A0`).
pub const NBSP_B64: &str = "w4LCoA==";

// ── Atom accessors ────────────────────────────────────────────────────────────

/// Return `cafÃ©` as a `String`.  See [`CAFE_B64`].
pub fn cafe() -> String {
    s(CAFE_B64)
}

/// Return `Ã©` as a `String`.  See [`LATIN1_FRAGMENT_B64`].
pub fn latin1_fragment() -> String {
    s(LATIN1_FRAGMENT_B64)
}

/// Return `Ã¨` as a `String`.  See [`E_GRAVE_B64`].
pub fn e_grave() -> String {
    s(E_GRAVE_B64)
}

/// Return `naÃ¯ve` as a `String`.  See [`NAIVE_B64`].
pub fn naive() -> String {
    s(NAIVE_B64)
}

/// Return the em-dash mojibake fingerprint as a `String`.  See [`EM_DASH_B64`].
pub fn em_dash() -> String {
    s(EM_DASH_B64)
}

/// Return the box-drawing mojibake fingerprint as a `String`.
/// See [`BOX_DRAWING_B64`].
pub fn box_drawing() -> String {
    s(BOX_DRAWING_B64)
}

/// Return `Â<NBSP>` as a `String`.  See [`NBSP_B64`].
pub fn nbsp() -> String {
    s(NBSP_B64)
}

// ── Composite accessors ───────────────────────────────────────────────────────
//
// Convenience wrappers that combine atoms with literal ASCII context
// the test sites care about.  Defined here (rather than at each call
// site) so the *bytes* never appear inline in test source.

/// `"<cafÃ©>\n"` — single-line Latin-1 mojibake plus LF.
pub fn cafe_line() -> String {
    let mut s = cafe();
    s.push('\n');
    s
}

/// All four canonical patterns mixed into one string, space-separated:
/// `cafÃ© â€\u{201D} â\u{201D}\u{20AC} Â\u{00A0}`.
pub fn all_four_mixed() -> String {
    format!("{} {} {} {}", cafe(), em_dash(), box_drawing(), nbsp())
}

/// Doubly-mojibake'd `café` — `cafÃƒÂ©` — which our M1 detector
/// correctly *fails* to flag because the second-layer prefix is `Â`,
/// not `Ã`.  Bytes: `63 61 66 C3 83 C6 92 C3 82 C2 A9`.
pub const DOUBLE_CAFE_B64: &str = "Y2Fmw4PGksOCwqk=";
// (Note: this constant is here mainly so a reader can see the two-layer
// fingerprint documented; only doctor's `double_mojibake` fixture
// actually uses it.)

/// Return the doubly-mojibake'd `café`-style fingerprint as a `String`.
pub fn double_cafe() -> String {
    s(DOUBLE_CAFE_B64)
}

#[cfg(test)]
mod selftests {
    //! Each constant decodes to the exact UTF-8 byte sequence its
    //! docstring promises.  These tests are the canonical guard
    //! against silent corruption of the fixtures.

    use super::*;

    fn assert_bytes(actual: &str, expected: &[u8], label: &str) {
        let got = b64(actual);
        assert_eq!(
            got, expected,
            "{label}: base64 decoded to wrong bytes\n  got: {got:02X?}\n  expected: {expected:02X?}"
        );
    }

    #[test]
    fn cafe_b64_decodes_correctly() {
        assert_bytes(CAFE_B64, &[0x63, 0x61, 0x66, 0xC3, 0x83, 0xC2, 0xA9], "CAFE_B64");
    }

    #[test]
    fn latin1_fragment_b64_decodes_correctly() {
        assert_bytes(LATIN1_FRAGMENT_B64, &[0xC3, 0x83, 0xC2, 0xA9], "LATIN1_FRAGMENT_B64");
    }

    #[test]
    fn e_grave_b64_decodes_correctly() {
        assert_bytes(E_GRAVE_B64, &[0xC3, 0x83, 0xC2, 0xA8], "E_GRAVE_B64");
    }

    #[test]
    fn naive_b64_decodes_correctly() {
        assert_bytes(
            NAIVE_B64,
            &[0x6E, 0x61, 0xC3, 0x83, 0xC2, 0xAF, 0x76, 0x65],
            "NAIVE_B64",
        );
    }

    #[test]
    fn em_dash_b64_decodes_correctly() {
        assert_bytes(
            EM_DASH_B64,
            &[0xC3, 0xA2, 0xE2, 0x82, 0xAC, 0xE2, 0x80, 0x9D],
            "EM_DASH_B64",
        );
    }

    #[test]
    fn box_drawing_b64_decodes_correctly() {
        assert_bytes(
            BOX_DRAWING_B64,
            &[0xC3, 0xA2, 0xE2, 0x80, 0x9D, 0xE2, 0x82, 0xAC],
            "BOX_DRAWING_B64",
        );
    }

    #[test]
    fn nbsp_b64_decodes_correctly() {
        assert_bytes(NBSP_B64, &[0xC3, 0x82, 0xC2, 0xA0], "NBSP_B64");
    }

    #[test]
    fn double_cafe_b64_decodes_correctly() {
        assert_bytes(
            DOUBLE_CAFE_B64,
            &[0x63, 0x61, 0x66, 0xC3, 0x83, 0xC6, 0x92, 0xC3, 0x82, 0xC2, 0xA9],
            "DOUBLE_CAFE_B64",
        );
    }

    #[test]
    fn all_fixture_strings_are_valid_utf8() {
        // Touch every accessor so a regression that produces invalid
        // UTF-8 trips immediately (the `s()` helper would panic).
        let _ = (
            cafe(),
            latin1_fragment(),
            e_grave(),
            naive(),
            em_dash(),
            box_drawing(),
            nbsp(),
            cafe_line(),
            all_four_mixed(),
            double_cafe(),
        );
    }
}
