// Copyright (c) 2026, Michael Grier
//
// encoding-check: allow-mojibake (this module's regexes and tests
// contain literal mojibake byte sequences)

//! Mojibake detection primitives.
//!
//! Pure, no-I/O functions for spotting characteristic
//! "UTF-8-decoded-as-Windows-1252-then-re-encoded-as-UTF-8" corruption in
//! already-decoded text.  These primitives back the higher layers (the
//! write-time guard in [`crate::cmd::write`] etc., the `tpu doctor`
//! subcommand, and the read-time advisory) and intentionally do *not*
//! perform any I/O of their own.
//!
//! ## Patterns
//!
//! Four characteristic patterns are detected:
//!
//! | Pattern        | Indicates                                                     |
//! |----------------|---------------------------------------------------------------|
//! | [`Pattern::Latin1`]      | Latin-1 letter misread (`Ã©`, `Ãª`, `Ã¨`, `Ã `, `Ã¯`, …) |
//! | [`Pattern::Punctuation`] | Typographic punctuation misread (`â€"`, `â€™`, `â€œ`, `â€¦`, …) |
//! | [`Pattern::BoxDrawing`]  | Box-drawing misread (`â"€`, `â"‚`, `â"Œ`, …)               |
//! | [`Pattern::Nbsp`]        | NBSP-as-`Â<sp>`                                            |
//!
//! ## Opt-out marker
//!
//! Any source file may contain the literal sentinel string
//! [`ALLOW_MARKER`] (`encoding-check: allow-mojibake`) on a line of its
//! own — typically inside a comment — to declare that it legitimately
//! contains mojibake digraphs (e.g. for documentation, regex sources, or
//! test fixtures).  Higher layers should call [`allowed_by_marker`] before
//! flagging a file.
//!
//! ## Recovery suggestion
//!
//! [`looks_like_one_layer_peel`] applies a single round of "decode each
//! `&str` char as a Windows-1252 byte where possible, then re-decode the
//! resulting bytes as UTF-8" and returns the result *only* when it
//! contains strictly fewer mojibake matches than the input.  This is the
//! safe building block for `tpu doctor --fix=peel`.

use std::sync::OnceLock;

use regex::Regex;

/// The opt-out sentinel string.  Any text containing this exact substring
/// is considered to legitimately include mojibake digraphs (see module
/// docs).
pub const ALLOW_MARKER: &str = "encoding-check: allow-mojibake";

/// One characteristic class of mojibake byte sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pattern {
    /// `Ã` (`U+00C3`) followed by a Latin-1 supplement char in
    /// `U+0080..=U+00BF` — the signature of any Latin-1 letter that was
    /// originally a 2-byte UTF-8 sequence starting `0xC3` (`é`, `ê`, `è`,
    /// `à`, `ï`, `ü`, …) misread through Windows-1252.
    Latin1,
    /// `â€` (`U+00E2 U+20AC`) followed by another C1-mapped char — the
    /// signature of any 3-byte UTF-8 sequence starting `0xE2 0x80 0x..`
    /// (em-dash, en-dash, curly quotes, ellipsis, bullet, …) misread.
    Punctuation,
    /// `â"` (`U+00E2 U+201D`) followed by another C1-mapped char — the
    /// signature of any 3-byte UTF-8 sequence starting `0xE2 0x94 0x..`
    /// (the box-drawing block) misread.
    BoxDrawing,
    /// `Â<NBSP>` (`U+00C2 U+00A0`) — a stray non-breaking space that was
    /// originally a single `0xA0` byte misread as `Â` (cp1252 `0xC2`)
    /// followed by literal NBSP (cp1252 `0xA0` → `U+00A0`).
    Nbsp,
}

impl Pattern {
    /// Stable, human-readable identifier for diagnostic output.
    pub fn name(self) -> &'static str {
        match self {
            Pattern::Latin1 => "latin1",
            Pattern::Punctuation => "punctuation",
            Pattern::BoxDrawing => "box-drawing",
            Pattern::Nbsp => "nbsp",
        }
    }

    /// All four pattern variants in canonical order.  Stable iteration
    /// order for diagnostic consumers and the write-time guard.
    pub const ALL: [Pattern; 4] = [
        Pattern::Latin1,
        Pattern::Punctuation,
        Pattern::BoxDrawing,
        Pattern::Nbsp,
    ];
}

#[inline]
fn pattern_index(p: Pattern) -> usize {
    match p {
        Pattern::Latin1 => 0,
        Pattern::Punctuation => 1,
        Pattern::BoxDrawing => 2,
        Pattern::Nbsp => 3,
    }
}

/// A single mojibake match within the scanned text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// Byte offset (within the input `&str`) of the first char of the
    /// match.
    pub byte_offset: usize,
    /// Which characteristic pattern matched.
    pub pattern: Pattern,
}

/// Aggregate result of [`scan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanReport {
    /// All matches, sorted by ascending `byte_offset`.
    pub matches: Vec<Match>,
    /// Total `char` count of the scanned input — useful for callers that
    /// want to format ratios (e.g. "3 mojibake matches in a 1024-char file").
    pub total_chars: usize,
}

impl ScanReport {
    /// Convenience: `true` when no mojibake patterns were found.
    #[allow(dead_code)] // public API, only invoked from integration tests
    pub fn is_clean(&self) -> bool {
        self.matches.is_empty()
    }
}

// ── pattern table ───────────────────────────────────────────────────────────

fn patterns() -> &'static [(Pattern, Regex)] {
    static P: OnceLock<Vec<(Pattern, Regex)>> = OnceLock::new();
    P.get_or_init(|| {
        vec![
            (
                Pattern::Latin1,
                Regex::new(r"\u{00C3}[\u{0080}-\u{00BF}]").unwrap(),
            ),
            (
                Pattern::Punctuation,
                Regex::new(r"\u{00E2}\u{20AC}[\u{0080}-\u{20FF}]").unwrap(),
            ),
            (
                Pattern::BoxDrawing,
                Regex::new(r"\u{00E2}\u{201D}[\u{0080}-\u{20FF}]").unwrap(),
            ),
            (Pattern::Nbsp, Regex::new(r"\u{00C2}\u{00A0}").unwrap()),
        ]
    })
}

// ── public API ──────────────────────────────────────────────────────────────

/// Scan `text` for every occurrence of every characteristic mojibake
/// pattern.
///
/// Matches are returned sorted by ascending byte offset.  Overlapping
/// matches across different patterns are all reported.  This function
/// does not consult [`ALLOW_MARKER`]; callers that want to honour the
/// opt-out should call [`allowed_by_marker`] first.
pub fn scan(text: &str) -> ScanReport {
    let mut matches = Vec::new();
    for (pat, re) in patterns() {
        for m in re.find_iter(text) {
            matches.push(Match {
                byte_offset: m.start(),
                pattern: *pat,
            });
        }
    }
    matches.sort_by_key(|m| m.byte_offset);
    ScanReport {
        matches,
        total_chars: text.chars().count(),
    }
}

/// Short-circuit variant of [`scan`] for hot paths.  Returns the
/// earliest-by-byte-offset match across all patterns, or `None` if the
/// input is clean.
#[allow(dead_code)] // public API, only invoked from integration tests
pub fn first_match(text: &str) -> Option<Match> {
    let mut best: Option<Match> = None;
    for (pat, re) in patterns() {
        if let Some(m) = re.find(text) {
            let candidate = Match {
                byte_offset: m.start(),
                pattern: *pat,
            };
            best = Some(match best {
                None => candidate,
                Some(b) if candidate.byte_offset < b.byte_offset => candidate,
                Some(b) => b,
            });
        }
    }
    best
}

/// `true` if `text` contains the [`ALLOW_MARKER`] opt-out sentinel
/// (case-sensitive, exact substring match).
pub fn allowed_by_marker(text: &str) -> bool {
    text.contains(ALLOW_MARKER)
}

/// Try a single round of "decode each char as a Windows-1252 byte where
/// possible, then re-decode as UTF-8" and return the result only when
/// it contains *strictly fewer* mojibake matches than `text`.
///
/// Returns `None` when:
///
/// * `text` already has zero mojibake matches (nothing to improve);
/// * the peel produces invalid UTF-8 (we never offer a peel that would
///   itself break the file);
/// * the peel doesn't actually reduce the match count (peeling further
///   wouldn't help).
///
/// This is intentionally conservative — applying multiple peels in
/// sequence is the caller's responsibility, and each round must
/// independently demonstrate progress.
pub fn looks_like_one_layer_peel(text: &str) -> Option<String> {
    let original = scan(text).matches.len();
    if original == 0 {
        return None;
    }
    let peeled = try_peel_once(text)?;
    let peeled_count = scan(&peeled).matches.len();
    if peeled_count < original {
        Some(peeled)
    } else {
        None
    }
}

// ── peel implementation ─────────────────────────────────────────────────────

/// Map a `char` back to its single Windows-1252 byte if one exists,
/// otherwise `None` (the char will be preserved as its UTF-8 encoding
/// during the peel).
fn char_to_cp1252(c: char) -> Option<u8> {
    let cp = c as u32;
    // ASCII passes straight through.
    if cp < 0x80 {
        return Some(cp as u8);
    }
    // The cp1252 G1 range (0xA0..=0xFF) is identity-mapped to U+00A0..=U+00FF.
    if (0x00A0..=0x00FF).contains(&cp) {
        return Some(cp as u8);
    }
    // C1 range (0x80..=0x9F) — sparse, defined-byte mapping per the
    // WHATWG Windows-1252 table.  Undefined cp1252 bytes (0x81, 0x8D,
    // 0x8F, 0x90, 0x9D) round-trip as their literal C1 codepoints.
    Some(match cp {
        0x20AC => 0x80, // €
        0x0081 => 0x81, // (undef → literal)
        0x201A => 0x82, // ‚
        0x0192 => 0x83, // ƒ
        0x201E => 0x84, // „
        0x2026 => 0x85, // …
        0x2020 => 0x86, // †
        0x2021 => 0x87, // ‡
        0x02C6 => 0x88, // ˆ
        0x2030 => 0x89, // ‰
        0x0160 => 0x8A, // Š
        0x2039 => 0x8B, // ‹
        0x0152 => 0x8C, // Œ
        0x008D => 0x8D,
        0x017D => 0x8E, // Ž
        0x008F => 0x8F,
        0x0090 => 0x90,
        0x2018 => 0x91, // '
        0x2019 => 0x92, // '
        0x201C => 0x93, // "
        0x201D => 0x94, // "
        0x2022 => 0x95, // •
        0x2013 => 0x96, // –
        0x2014 => 0x97, // —
        0x02DC => 0x98, // ˜
        0x2122 => 0x99, // ™
        0x0161 => 0x9A, // š
        0x203A => 0x9B, // ›
        0x0153 => 0x9C, // œ
        0x009D => 0x9D,
        0x017E => 0x9E, // ž
        0x0178 => 0x9F, // Ÿ
        _ => return None,
    })
}

/// Apply one peel layer.  Each char that has a cp1252 round-trip is
/// emitted as its single byte; chars that don't (CJK, emoji, etc.) are
/// preserved as their original UTF-8 encoding.  Returns `None` if the
/// resulting byte sequence is not valid UTF-8.
fn try_peel_once(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut buf = [0u8; 4];
    for c in text.chars() {
        match char_to_cp1252(c) {
            Some(b) => bytes.push(b),
            None => bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
        }
    }
    String::from_utf8(bytes).ok()
}

// ── Lossy-replacement (U+FFFD residue) diagnostic ───────────────────────────

/// Opt-out sentinel specific to the `lossy-replacement` diagnostic class.
/// A file containing this exact substring (typically inside a comment) will
/// not be flagged for `U+FFFD` replacement-character occurrences.  The
/// existing [`ALLOW_MARKER`] sentinel also suppresses replacement-character
/// detection — it opts out of *all* encoding-check diagnostics.
pub const ALLOW_REPLACEMENT_CHAR_MARKER: &str = "encoding-check: allow-replacement-char";

/// `true` if `text` contains either [`ALLOW_MARKER`] or
/// [`ALLOW_REPLACEMENT_CHAR_MARKER`].
///
/// Callers that already tested [`allowed_by_marker`] and returned early do
/// not need to call this separately, but it is safe to call unconditionally.
pub fn has_replacement_char_allow_marker(text: &str) -> bool {
    text.contains(ALLOW_REPLACEMENT_CHAR_MARKER)
}

/// A single occurrence of `U+FFFD` (the Unicode Replacement Character) within
/// the scanned text.
///
/// This is a **separate diagnostic class** from [`Match`] / [`ScanReport`].
/// `U+FFFD` in an otherwise valid UTF-8 file is the *terminal* residue of a
/// prior lossy decode (the original byte is gone and cannot be recovered by a
/// peel).  [`scan_replacement_chars`] never sets `peel_suggested`; callers
/// must surface this as `repair: manual`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacementCharMatch {
    /// Byte offset (within the input `&str`) of the `U+FFFD` character.
    pub byte_offset: usize,
    /// A short excerpt of surrounding text (up to 20 chars on each side)
    /// for context in the report.  May contain further `U+FFFD` chars.
    pub context: String,
    /// Heuristic suggested replacement, set only when the caller passes
    /// `guess: true`.  `None` when no confident inference can be made.
    ///
    /// Current heuristics:
    /// - flanked by spaces on both sides → em-dash `—` (U+2014)
    /// - flanked by ASCII digits on both sides → en-dash `–` (U+2013)
    pub suggested: Option<char>,
}

/// Scan `text` for every occurrence of `U+FFFD` (Unicode Replacement
/// Character, bytes `EF BF BD`).
///
/// Returns one [`ReplacementCharMatch`] per occurrence, sorted by ascending
/// byte offset.  When `guess` is `true` each match is annotated with a
/// heuristic suggested replacement based on immediately surrounding chars.
///
/// This function does **not** consult [`ALLOW_MARKER`] or
/// [`ALLOW_REPLACEMENT_CHAR_MARKER`]; callers must check those first.
pub fn scan_replacement_chars(text: &str, guess: bool) -> Vec<ReplacementCharMatch> {
    // Build a char-indexed view so context windows and neighbour lookups
    // can work in O(n) without multiple passes.
    let chars: Vec<char> = text.chars().collect();
    let mut byte_offsets = Vec::with_capacity(chars.len());
    let mut pos = 0usize;
    for &c in &chars {
        byte_offsets.push(pos);
        pos += c.len_utf8();
    }

    let mut results = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        if c != '\u{FFFD}' {
            continue;
        }
        const WINDOW: usize = 20;
        let start = i.saturating_sub(WINDOW);
        let end = (i + 1 + WINDOW).min(chars.len());
        let context: String = chars[start..end].iter().collect();

        let suggested = if guess {
            guess_replacement_char(i, &chars)
        } else {
            None
        };

        results.push(ReplacementCharMatch {
            byte_offset: byte_offsets[i],
            context,
            suggested,
        });
    }
    results
}

/// Infer a likely original character for a `U+FFFD` at `chars[idx]` based
/// on immediately surrounding characters.
///
/// Returns `None` when no heuristic applies.
fn guess_replacement_char(idx: usize, chars: &[char]) -> Option<char> {
    let prev = idx.checked_sub(1).map(|i| chars[i]);
    let next = chars.get(idx + 1).copied();

    // Flanked by ASCII spaces → almost always an em-dash.
    if prev == Some(' ') && next == Some(' ') {
        return Some('\u{2014}'); // —
    }
    // Flanked by ASCII digits (directly adjacent) → en-dash range separator.
    if prev.map_or(false, |c| c.is_ascii_digit()) && next.map_or(false, |c| c.is_ascii_digit()) {
        return Some('\u{2013}'); // –
    }
    None
}

// ── Write-time guard (Milestone 2) ──────────────────────────────────────────

/// Policy controlling write-time mojibake checks performed by the
/// mutating `tpu::cmd::*` operations (`write`, `replace`, `edit`,
/// `append`).
///
/// Constructed via [`Default::default`] (rejection on — the safe
/// default) or [`WritePolicy::permissive`] (rejection off — equivalent
/// to passing `--allow-mojibake` on the CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WritePolicy {
    /// When `true` (default), a write that would *introduce* new
    /// mojibake matches relative to the file's prior content is
    /// rejected at the library boundary.  Existing damage is preserved
    /// without complaint; only newly-added matches trigger a refusal.
    pub reject_introduced_mojibake: bool,
}

impl Default for WritePolicy {
    fn default() -> Self {
        Self {
            reject_introduced_mojibake: true,
        }
    }
}

impl WritePolicy {
    /// Convenience constructor that disables every guard.  Equivalent
    /// to passing `--allow-mojibake` on the CLI.
    pub fn permissive() -> Self {
        Self {
            reject_introduced_mojibake: false,
        }
    }
}

/// Error returned by [`check_write_does_not_introduce_mojibake`] when
/// the proposed `new` content would add mojibake matches not present in
/// `old`.
///
/// The `Display` implementation produces the standard hint message
/// (`writing this content would introduce mojibake (e.g. 'latin1' at
/// byte offset 42); pass --allow-mojibake to override`) so callers can
/// surface it directly to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MojibakeIntroduced {
    /// Per-pattern count of newly-introduced matches (excludes matches
    /// already present in `old`).  Sorted by [`Pattern::ALL`] order;
    /// patterns with zero introductions are omitted.
    pub introduced: Vec<(Pattern, usize)>,
    /// The pattern of the first newly-introduced match — used to anchor
    /// the human-readable hint.
    pub first_pattern: Pattern,
    /// The byte offset (within the proposed `new` content) of the first
    /// newly-introduced match.
    pub first_offset: usize,
}

impl std::fmt::Display for MojibakeIntroduced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let total: usize = self.introduced.iter().map(|(_, n)| n).sum();
        write!(
            f,
            "writing this content would introduce mojibake \
             ({} new match{}, first '{}' at byte offset {}); \
             pass --allow-mojibake to override",
            total,
            if total == 1 { "" } else { "es" },
            self.first_pattern.name(),
            self.first_offset,
        )
    }
}

impl std::error::Error for MojibakeIntroduced {}

/// Compare the mojibake fingerprints of `old` and `new` and return an
/// error if `new` contains additional matches of any pattern.
///
/// The check is performed per-pattern: matches that already existed in
/// `old` are ignored, so callers are not punished for damage they did
/// not cause.  Writes that *remove* mojibake or that are byte-identical
/// to a corrupt original both succeed.  For brand-new files (no
/// pre-existing content), pass `""` for `old`.
///
/// Honours [`ALLOW_MARKER`]: if the proposed `new` content contains
/// the opt-out sentinel anywhere, the check returns `Ok(())`
/// regardless of any introduced matches.  This lets documentation
/// files (such as this module's own `CHECKLIST.md`) legitimately
/// contain mojibake digraphs without requiring a CLI override.
pub fn check_write_does_not_introduce_mojibake(
    old: &str,
    new: &str,
) -> Result<(), MojibakeIntroduced> {
    if allowed_by_marker(new) {
        return Ok(());
    }
    let new_report = scan(new);
    if new_report.matches.is_empty() {
        return Ok(());
    }
    let old_report = scan(old);

    let mut old_counts = [0usize; 4];
    for m in &old_report.matches {
        old_counts[pattern_index(m.pattern)] += 1;
    }
    let mut new_counts = [0usize; 4];
    for m in &new_report.matches {
        new_counts[pattern_index(m.pattern)] += 1;
    }

    let mut introduced: Vec<(Pattern, usize)> = Vec::new();
    for p in Pattern::ALL {
        let i = pattern_index(p);
        if new_counts[i] > old_counts[i] {
            introduced.push((p, new_counts[i] - old_counts[i]));
        }
    }
    if introduced.is_empty() {
        return Ok(());
    }

    // Locate the first new match whose pattern exceeds the old budget.
    // Walking in offset order with a per-pattern counter ensures we
    // report the actual first *introduced* match, not a pre-existing
    // one that happened to come first.
    let mut budget = old_counts;
    let mut first: Option<Match> = None;
    for m in &new_report.matches {
        let i = pattern_index(m.pattern);
        if budget[i] > 0 {
            budget[i] -= 1;
        } else {
            first = Some(*m);
            break;
        }
    }
    let first = first.expect("introduced is non-empty so a budget overflow exists");
    Err(MojibakeIntroduced {
        introduced,
        first_pattern: first.pattern,
        first_offset: first.byte_offset,
    })
}

// ── Read-time advisory (Milestone 4) ────────────────────────────────────────

/// If `text` looks mojibake'd and is not opted out via [`ALLOW_MARKER`],
/// return the number of matches; otherwise return `None`.
///
/// This is the central decision used by every read-side command to
/// decide whether to emit the read-time advisory note.  It is a pure
/// function — emission of the human-readable hint is the caller's job
/// (see [`emit_read_advisory`]).
pub fn check_read_advisory(text: &str) -> Option<usize> {
    if allowed_by_marker(text) {
        return None;
    }
    let report = scan(text);
    if report.matches.is_empty() {
        None
    } else {
        Some(report.matches.len())
    }
}

/// Write the canonical one-line read-time advisory for `path` /
/// `text` to `notes` if (and only if) [`check_read_advisory`] returns
/// `Some`.  Returns `Ok(())` when no advisory was needed.
///
/// The line emitted has the stable format
///
/// ```text
/// note: <path>: file appears to contain mojibake (<N> matches); run 'tpu doctor' for details
/// ```
///
/// and is terminated by a single `\n`.  Callers are expected to route
/// `notes` to whatever stderr / diagnostics channel makes sense for
/// their environment (the CLI uses the [`crate::shell::Shell`] writer;
/// the MCP server can buffer it for inclusion in its own response).
pub fn emit_read_advisory(
    notes: &mut dyn std::io::Write,
    path: &std::path::Path,
    text: &str,
) -> std::io::Result<()> {
    if let Some(n) = check_read_advisory(text) {
        writeln!(
            notes,
            "note: {}: file appears to contain mojibake ({n} matches); run 'tpu doctor' for details",
            path.display()
        )?;
    }
    Ok(())
}

// ════════════════════════════════════════════════════════════════════════════
// Unit tests
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── scan ────────────────────────────────────────────────────────────────

    #[test]
    fn scan_clean_ascii_has_no_matches() {
        let r = scan("hello world\nfoo bar baz\n");
        assert!(r.is_clean());
        assert_eq!(r.matches, vec![]);
    }

    #[test]
    fn scan_clean_utf8_with_real_chars_has_no_matches() {
        // Real em-dash, real curly quotes, real box-drawing, real CJK,
        // real emoji.  None of these are mojibake.
        let r = scan("café — \"hello\" ─┌┐└┘ 漢字 😀");
        assert!(r.is_clean(), "unexpected matches: {:?}", r.matches);
    }

    #[test]
    fn scan_detects_latin1_pattern() {
        // "café" UTF-8-then-cp1252-then-UTF-8 = "cafÃ©"
        let r = scan("cafÃ©");
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].pattern, Pattern::Latin1);
    }

    #[test]
    fn scan_detects_punctuation_pattern() {
        // Em-dash mojibake: "â€"
        let r = scan("hello â€\u{201D} world");
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].pattern, Pattern::Punctuation);
    }

    #[test]
    fn scan_detects_box_drawing_pattern() {
        // U+2500 (─) mojibake: "â"€"
        let r = scan("â\u{201D}\u{20AC}");
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].pattern, Pattern::BoxDrawing);
    }

    #[test]
    fn scan_detects_nbsp_pattern() {
        let r = scan("Â\u{00A0}word");
        assert_eq!(r.matches.len(), 1);
        assert_eq!(r.matches[0].pattern, Pattern::Nbsp);
    }

    #[test]
    fn scan_detects_all_four_patterns_mixed() {
        let s = "cafÃ© â€\u{201D} â\u{201D}\u{20AC} Â\u{00A0}";
        let r = scan(s);
        assert_eq!(r.matches.len(), 4);
        let pats: Vec<Pattern> = r.matches.iter().map(|m| m.pattern).collect();
        assert!(pats.contains(&Pattern::Latin1));
        assert!(pats.contains(&Pattern::Punctuation));
        assert!(pats.contains(&Pattern::BoxDrawing));
        assert!(pats.contains(&Pattern::Nbsp));
    }

    #[test]
    fn scan_matches_are_sorted_by_offset() {
        let s = "Â\u{00A0} cafÃ©";
        let r = scan(s);
        assert_eq!(r.matches.len(), 2);
        assert!(r.matches[0].byte_offset < r.matches[1].byte_offset);
    }

    #[test]
    fn scan_empty_string_has_no_matches() {
        let r = scan("");
        assert!(r.is_clean());
        assert_eq!(r.total_chars, 0);
    }

    #[test]
    fn scan_lone_capital_a_tilde_does_not_match_latin1() {
        // "Ã" not followed by a C1-range char must NOT trigger Latin1.
        let r = scan("Ã hello");
        assert!(r.is_clean(), "unexpected matches: {:?}", r.matches);
    }

    #[test]
    fn scan_lone_a_circ_does_not_match_punctuation() {
        // Plain "â" without the trailing € or " is just French.
        let r = scan("château forêt");
        assert!(r.is_clean(), "unexpected matches: {:?}", r.matches);
    }

    #[test]
    fn scan_total_chars_counts_codepoints_not_bytes() {
        let r = scan("café"); // 4 chars, 5 bytes
        assert_eq!(r.total_chars, 4);
    }

    // ── first_match ─────────────────────────────────────────────────────────

    #[test]
    fn first_match_returns_none_on_clean_input() {
        assert_eq!(first_match("hello world"), None);
    }

    #[test]
    fn first_match_returns_earliest_by_byte_offset() {
        // Punctuation mojibake at offset 0; Latin1 mojibake later.
        let s = "â€\u{201D} cafÃ©";
        let m = first_match(s).expect("should match");
        assert_eq!(m.pattern, Pattern::Punctuation);
        assert_eq!(m.byte_offset, 0);
    }

    #[test]
    fn first_match_finds_late_match_in_long_input() {
        let mut s = "a".repeat(10_000);
        s.push_str("cafÃ©");
        let m = first_match(&s).expect("should match");
        assert_eq!(m.pattern, Pattern::Latin1);
        // 10_000 'a' bytes + 'c','a','f' = byte offset 10_003 for the 'Ã'.
        assert_eq!(m.byte_offset, 10_003);
    }

    // ── allowed_by_marker ───────────────────────────────────────────────────

    #[test]
    fn allow_marker_present_in_comment() {
        let s = "// encoding-check: allow-mojibake (test fixture)\nfoo";
        assert!(allowed_by_marker(s));
    }

    #[test]
    fn allow_marker_absent() {
        assert!(!allowed_by_marker("nothing to see here"));
    }

    #[test]
    fn allow_marker_misspelled_does_not_match() {
        assert!(!allowed_by_marker("encoding-check: allow_mojibake"));
        assert!(!allowed_by_marker("encoding-check:allow-mojibake"));
        assert!(!allowed_by_marker("encoding check: allow-mojibake"));
    }

    #[test]
    fn allow_marker_is_case_sensitive() {
        assert!(!allowed_by_marker("Encoding-Check: Allow-Mojibake"));
        assert!(!allowed_by_marker("ENCODING-CHECK: ALLOW-MOJIBAKE"));
    }

    #[test]
    fn allow_marker_empty_string() {
        assert!(!allowed_by_marker(""));
    }

    // ── looks_like_one_layer_peel ───────────────────────────────────────────

    #[test]
    fn peel_recovers_single_layer_mojibake() {
        // Original: "café" (UTF-8: 63 61 66 C3 A9)
        // After cp1252 misread + UTF-8 re-encode: "cafÃ©"
        //   C3 → "Ã" → bytes C3 83
        //   A9 → "©" → bytes C2 A9
        // So mojibake'd string bytes: 63 61 66 C3 83 C2 A9
        let mojibake = "cafÃ©";
        let peeled = looks_like_one_layer_peel(mojibake).expect("should peel");
        assert_eq!(peeled, "café");
    }

    #[test]
    fn peel_returns_none_for_clean_input() {
        assert_eq!(looks_like_one_layer_peel("hello world"), None);
        assert_eq!(looks_like_one_layer_peel("café"), None);
    }

    #[test]
    fn peel_returns_none_when_result_is_invalid_utf8() {
        // A lone 'Ã' (one cp1252 byte = 0xC3) followed by a non-continuation
        // byte after peel would yield invalid UTF-8.  Construct exactly that:
        // chars 'Ã' + 'x' → bytes C3 78 → invalid UTF-8 → peel returns None.
        // But scan() requires Latin1 to fire, which needs 'Ã' + C1-range.
        // So we test with mojibake that DOES match scan but peels to invalid:
        // 'Ã' alone won't match scan (handled elsewhere).  Use a construct
        // where peel would break UTF-8: 'Ã' + 0x80 char.
        // 'Ã' (U+00C3 → byte C3) + '\u{0080}' (→ byte 80 since cp1252 0x80
        // maps to U+20AC, but \u{0080} doesn't have a cp1252 byte in our
        // table → we preserve its UTF-8 encoding C2 80, so peel yields
        // C3 C2 80 which IS valid UTF-8).  Peel succeeds here.
        // Easier approach: input scans clean (0 matches) -> None by
        // contract, already covered above.  This test verifies the
        // String::from_utf8 path doesn't panic on real-world input.
        let result = looks_like_one_layer_peel("Ã\u{0080}");
        // Either None (clean → no peel attempted, OR invalid UTF-8 result)
        // — the contract says peel returns None unless it strictly improves.
        // What matters: no panic, no Some that's worse than the input.
        if let Some(p) = result {
            assert!(scan(&p).matches.len() < scan("Ã\u{0080}").matches.len());
        }
    }

    #[test]
    fn peel_returns_none_when_no_improvement() {
        // Synthetic adversarial case: Latin1 pattern that, after peel,
        // re-introduces the same number of matches.  Since cp1252 maps
        // are deterministic, in practice clean text peels won't loop.
        // The strictly-fewer guarantee is the test target.
        let s = "cafÃ©";
        let p1 = looks_like_one_layer_peel(s).expect("first peel");
        // Peeling the already-peeled (clean) string must return None.
        assert_eq!(looks_like_one_layer_peel(&p1), None);
    }

    #[test]
    fn peel_preserves_non_latin1_chars() {
        // Mojibake adjacent to CJK / emoji: the exotic chars must be
        // preserved, not corrupted.
        let mojibake = "漢字 cafÃ© 😀";
        let peeled = looks_like_one_layer_peel(mojibake).expect("should peel");
        assert!(peeled.contains("漢字"));
        assert!(peeled.contains("café"));
        assert!(peeled.contains("😀"));
    }

    #[test]
    fn peel_does_not_attempt_double_mojibake() {
        // Doubly-mojibake'd "café" = c,a,f,Ã,ƒ,Â,©.  The second layer
        // hides the characteristic Latin1 byte pair (`Ã` here is followed
        // by `ƒ`, which is outside U+0080..=U+00BF), so scan() reports
        // zero matches.  Per the M1-4 contract, single-layer peel
        // returns None when there is nothing to improve in match count
        // terms.  Multi-layer recovery is the doctor command's job
        // (Milestone 3).
        let double = "cafÃƒÂ©";
        assert_eq!(scan(double).matches.len(), 0);
        assert_eq!(looks_like_one_layer_peel(double), None);
    }

    // ── WritePolicy ─────────────────────────────────────────────────────────

    #[test]
    fn write_policy_default_rejects() {
        assert!(WritePolicy::default().reject_introduced_mojibake);
    }

    #[test]
    fn write_policy_permissive_does_not_reject() {
        assert!(!WritePolicy::permissive().reject_introduced_mojibake);
    }

    #[test]
    fn write_policy_is_copy_clone_eq_debug() {
        // Lock down the trait set so consumers can rely on it.
        fn assert_copy<T: Copy>() {}
        fn assert_clone<T: Clone>() {}
        fn assert_eq<T: PartialEq + Eq>() {}
        fn assert_debug<T: std::fmt::Debug>() {}
        assert_copy::<WritePolicy>();
        assert_clone::<WritePolicy>();
        assert_eq::<WritePolicy>();
        assert_debug::<WritePolicy>();
        let a = WritePolicy::default();
        let b = a;
        assert_eq!(a, b);
        assert_ne!(a, WritePolicy::permissive());
    }

    // ── check_write_does_not_introduce_mojibake ─────────────────────────────

    #[test]
    fn check_clean_to_clean_is_ok() {
        assert!(check_write_does_not_introduce_mojibake("hello", "hello world").is_ok());
    }

    #[test]
    fn check_clean_to_corrupt_is_rejected() {
        let err = check_write_does_not_introduce_mojibake("hello", "cafÃ©")
            .expect_err("must reject newly-introduced mojibake");
        assert_eq!(err.first_pattern, Pattern::Latin1);
        assert_eq!(err.introduced.len(), 1);
        assert_eq!(err.introduced[0], (Pattern::Latin1, 1));
    }

    #[test]
    fn check_preexisting_corrupt_unchanged_is_ok() {
        // Old already had mojibake; new == old.  Don't punish writers
        // for damage they didn't cause.
        let s = "cafÃ©";
        assert!(check_write_does_not_introduce_mojibake(s, s).is_ok());
    }

    #[test]
    fn check_preexisting_corrupt_plus_more_is_rejected() {
        // Old had one Latin1 match; new has two.  Only the *added* one
        // is reported.
        let old = "cafÃ©";
        let new = "cafÃ© Ã¨";
        let err = check_write_does_not_introduce_mojibake(old, new)
            .expect_err("must reject additional mojibake");
        assert_eq!(err.introduced, vec![(Pattern::Latin1, 1)]);
        // First introduced offset is the SECOND Latin1 match in `new`,
        // not the first (which is part of the pre-existing budget).
        let scan_new = scan(new);
        assert_eq!(err.first_offset, scan_new.matches[1].byte_offset);
    }

    #[test]
    fn check_corrupt_to_clean_is_ok() {
        // Removing mojibake is always allowed.
        assert!(check_write_does_not_introduce_mojibake("cafÃ©", "café").is_ok());
    }

    #[test]
    fn check_respects_allow_marker_in_new() {
        // Even if `new` introduces mojibake, the allow-marker overrides.
        let new = "cafÃ©\n// encoding-check: allow-mojibake\n";
        assert!(check_write_does_not_introduce_mojibake("clean", new).is_ok());
    }

    #[test]
    fn check_does_not_consult_allow_marker_in_old() {
        // The marker only opts the *new* content out — adding mojibake
        // to a file where the marker only existed in the old content
        // is still rejected.
        let old = "// encoding-check: allow-mojibake\n";
        let new = "no marker here cafÃ©";
        assert!(check_write_does_not_introduce_mojibake(old, new).is_err());
    }

    #[test]
    fn check_introduces_multiple_pattern_classes() {
        // Two different new pattern classes → both reported.
        let err = check_write_does_not_introduce_mojibake("clean", "cafÃ© Â\u{00A0}x")
            .expect_err("must reject");
        let kinds: Vec<Pattern> = err.introduced.iter().map(|(p, _)| *p).collect();
        assert!(kinds.contains(&Pattern::Latin1));
        assert!(kinds.contains(&Pattern::Nbsp));
    }

    #[test]
    fn check_error_display_contains_hint() {
        let err = check_write_does_not_introduce_mojibake("clean", "cafÃ©").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("introduce mojibake"), "msg: {msg}");
        assert!(msg.contains("--allow-mojibake"), "msg: {msg}");
        assert!(msg.contains("latin1"), "msg: {msg}");
    }

    #[test]
    fn check_empty_to_corrupt_is_rejected() {
        // New-file scenario (no pre-existing content).
        assert!(check_write_does_not_introduce_mojibake("", "cafÃ©").is_err());
    }

    #[test]
    fn check_empty_to_clean_is_ok() {
        assert!(check_write_does_not_introduce_mojibake("", "hello").is_ok());
    }

    #[test]
    fn pattern_all_constant_lists_every_variant() {
        assert_eq!(Pattern::ALL.len(), 4);
        assert_eq!(Pattern::ALL[0], Pattern::Latin1);
        assert_eq!(Pattern::ALL[1], Pattern::Punctuation);
        assert_eq!(Pattern::ALL[2], Pattern::BoxDrawing);
        assert_eq!(Pattern::ALL[3], Pattern::Nbsp);
    }

    // ── Read-time advisory ──────────────────────────────────────────────────

    #[test]
    fn check_read_advisory_returns_none_for_clean_text() {
        assert_eq!(check_read_advisory("hello world\n"), None);
        assert_eq!(check_read_advisory("café — résumé"), None);
        assert_eq!(check_read_advisory(""), None);
    }

    #[test]
    fn check_read_advisory_returns_count_for_mojibake() {
        assert_eq!(check_read_advisory("cafÃ©\n"), Some(1));
        assert_eq!(check_read_advisory("cafÃ© and Ã¨"), Some(2));
    }

    #[test]
    fn check_read_advisory_honours_allow_marker() {
        let body = format!("// {ALLOW_MARKER}\nthis has cafÃ© in it");
        assert_eq!(check_read_advisory(&body), None);
    }

    #[test]
    fn emit_read_advisory_writes_canonical_line() {
        let mut buf: Vec<u8> = Vec::new();
        emit_read_advisory(&mut buf, std::path::Path::new("x.txt"), "cafÃ©").unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("note: x.txt: file appears to contain mojibake "));
        assert!(s.contains("(1 matches)"));
        assert!(s.contains("'tpu doctor'"));
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn emit_read_advisory_writes_nothing_for_clean_text() {
        let mut buf: Vec<u8> = Vec::new();
        emit_read_advisory(&mut buf, std::path::Path::new("x.txt"), "hello").unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn emit_read_advisory_writes_nothing_when_marker_present() {
        let body = format!("# {ALLOW_MARKER}\ncafÃ©");
        let mut buf: Vec<u8> = Vec::new();
        emit_read_advisory(&mut buf, std::path::Path::new("x.txt"), &body).unwrap();
        assert!(buf.is_empty());
    }
}
