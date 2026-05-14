// Copyright (c) 2026, Michael Grier

//! Integration test for Milestone 1 — `tpu::mojibake` detection
//! primitives.
//!
//! This test feeds the `scan`, `first_match`, `allowed_by_marker`, and
//! `looks_like_one_layer_peel` functions a curated corpus of fixture
//! strings covering every real-world condition the upper layers
//! (write-time guard, `tpu doctor`, read-time advisory) will rely on:
//!
//! * clean ASCII
//! * clean UTF-8 with em-dashes / box-drawing / curly quotes / CJK / emoji
//! * single-mojibake (each pattern in isolation, plus all four mixed)
//! * doubly-mojibake (silently scans clean — peel correctly declines)
//! * mojibake content with the explicit allow-marker opt-out
//!
//! All literal mojibake byte sequences come from
//! [`tpu::test_fixtures`] (decoded from base-64 at runtime) so this
//! source file remains pure ASCII and needs no `allow-mojibake` opt-out.

use tpu::mojibake::{
    self, ALLOW_MARKER, Pattern, allowed_by_marker, first_match, looks_like_one_layer_peel, scan,
};
use tpu::test_fixtures::{all_four_mixed, box_drawing, cafe, double_cafe, em_dash, naive, nbsp};

/// Each fixture: a label, the input text, the expected mojibake match
/// count from `scan`, and the expected allow-marker honouring.
struct Fixture {
    label: &'static str,
    text: String,
    expected_matches: usize,
    expected_allow_marker: bool,
}

fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            label: "clean ASCII",
            text: "Hello, world!\nThis file is plain ASCII.\n".to_string(),
            expected_matches: 0,
            expected_allow_marker: false,
        },
        Fixture {
            label: "clean UTF-8 with real punctuation, box-drawing, CJK, emoji",
            // café — naïve résumé · "hi" ─┌┐└┘├┤ 漢字 😀 🚀
            text: "caf\u{00E9} \u{2014} na\u{00EF}ve r\u{00E9}sum\u{00E9} \u{00B7} \
                   \u{201C}hi\u{201D} \u{2500}\u{250C}\u{2510}\u{2514}\u{2518}\u{251C}\u{2524} \
                   \u{6F22}\u{5B57} \u{1F600} \u{1F680}"
                .to_string(),
            expected_matches: 0,
            expected_allow_marker: false,
        },
        Fixture {
            label: "single-mojibake: Latin-1 only",
            text: format!("Today's special: {} au lait", cafe()),
            expected_matches: 1,
            expected_allow_marker: false,
        },
        Fixture {
            label: "single-mojibake: punctuation em-dash",
            text: format!("title {} subtitle", em_dash()),
            expected_matches: 1,
            expected_allow_marker: false,
        },
        Fixture {
            label: "single-mojibake: box-drawing",
            text: format!("border: {}{}", box_drawing(), box_drawing()),
            expected_matches: 2,
            expected_allow_marker: false,
        },
        Fixture {
            label: "single-mojibake: NBSP",
            text: format!("value:{}42", nbsp()),
            expected_matches: 1,
            expected_allow_marker: false,
        },
        Fixture {
            label: "all four patterns mixed in one string",
            text: all_four_mixed(),
            expected_matches: 4,
            expected_allow_marker: false,
        },
        Fixture {
            label: "doubly-mojibake'd cafe (silently scans clean)",
            text: format!("the bistro: {} today", double_cafe()),
            expected_matches: 0,
            expected_allow_marker: false,
        },
        Fixture {
            label: "mojibake content guarded by allow-marker",
            text: format!("// {} (test fixture)\n{}\n", ALLOW_MARKER, cafe()),
            // scan still reports them; allow_by_marker is the opt-out
            expected_matches: 1,
            expected_allow_marker: true,
        },
    ]
}

#[test]
fn scan_classifies_each_fixture_correctly() {
    for fx in fixtures() {
        let r = scan(&fx.text);
        assert_eq!(
            r.matches.len(),
            fx.expected_matches,
            "fixture {:?}: expected {} matches, got {} ({:?})",
            fx.label,
            fx.expected_matches,
            r.matches.len(),
            r.matches,
        );
    }
}

#[test]
fn first_match_agrees_with_scan_on_each_fixture() {
    for fx in fixtures() {
        let r = scan(&fx.text);
        let fm = first_match(&fx.text);
        match r.matches.first() {
            None => assert!(
                fm.is_none(),
                "fixture {:?}: scan empty but first_match returned {:?}",
                fx.label,
                fm,
            ),
            Some(expected) => {
                let got = fm.expect("first_match should be Some when scan has matches");
                assert_eq!(
                    got.byte_offset, expected.byte_offset,
                    "fixture {:?}: first_match offset disagrees with scan",
                    fx.label,
                );
                assert_eq!(
                    got.pattern, expected.pattern,
                    "fixture {:?}: first_match pattern disagrees with scan",
                    fx.label,
                );
            }
        }
    }
}

#[test]
fn allow_marker_correctly_recognised_for_each_fixture() {
    for fx in fixtures() {
        assert_eq!(
            allowed_by_marker(&fx.text),
            fx.expected_allow_marker,
            "fixture {:?}: allow-marker recognition mismatch",
            fx.label,
        );
    }
}

#[test]
fn peel_strictly_improves_or_returns_none_for_each_fixture() {
    for fx in fixtures() {
        let before = scan(&fx.text).matches.len();
        match looks_like_one_layer_peel(&fx.text) {
            None => {
                // Per contract: None means either nothing-to-improve or
                // peel would not strictly reduce match count.
                // Both are acceptable; nothing else to assert here.
            }
            Some(peeled) => {
                let after = scan(&peeled).matches.len();
                assert!(
                    after < before,
                    "fixture {:?}: peel returned Some but did not strictly \
                     reduce match count ({} -> {}): {:?}",
                    fx.label,
                    before,
                    after,
                    peeled,
                );
            }
        }
    }
}

#[test]
fn peel_recovers_canonical_single_layer_mojibake() {
    // The canonical end-to-end demonstration: a Latin-1-only mojibake
    // string round-trips back to its original UTF-8 form.
    // Original: "café — naïve"
    let original = "caf\u{00E9} \u{2014} na\u{00EF}ve";
    let mojibake = format!("{} {} {}", cafe(), em_dash(), naive());
    assert!(scan(&mojibake).matches.len() >= 2);
    let peeled = looks_like_one_layer_peel(&mojibake)
        .expect("M1-4 should improve a clearly-mojibake'd string");
    assert_eq!(peeled, original);
    assert!(scan(&peeled).is_clean());
}

#[test]
fn allow_marker_constant_value_is_stable() {
    // Keep the string in lockstep with tools/check-encoding.ps1 — if you
    // change one, change the other.
    assert_eq!(ALLOW_MARKER, "encoding-check: allow-mojibake");
}

#[test]
fn pattern_names_are_stable_for_diagnostic_consumers() {
    // Higher layers (tpu doctor JSON output, MCP error messages) will
    // serialise these names; lock them down.
    assert_eq!(Pattern::Latin1.name(), "latin1");
    assert_eq!(Pattern::Punctuation.name(), "punctuation");
    assert_eq!(Pattern::BoxDrawing.name(), "box-drawing");
    assert_eq!(Pattern::Nbsp.name(), "nbsp");
}

#[test]
fn module_is_pure_and_does_not_panic_on_pathological_inputs() {
    // Long input full of high-codepoint chars must not panic and must
    // run in reasonable time.  We intentionally don't assert match
    // count — the goal is to prove the API is robust.
    let long = "\u{6F22}\u{5B57}\u{1F600}".repeat(50_000);
    let _ = scan(&long);
    let _ = first_match(&long);
    let _ = looks_like_one_layer_peel(&long);

    // Single-character / boundary inputs.
    for s in [
        "", "a", "\u{00C3}", "\u{00E2}", "\u{00C2}", "\u{00A0}", "\u{20AC}",
    ] {
        let _ = scan(s);
        let _ = first_match(s);
        let _ = looks_like_one_layer_peel(s);
        let _ = mojibake::allowed_by_marker(s);
    }
}
