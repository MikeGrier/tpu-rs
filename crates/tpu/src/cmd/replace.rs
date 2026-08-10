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
//!
//! ## Regex is opt-in
//!
//! By default `pattern` is treated as a fixed literal string (every regex
//! metacharacter is escaped via [`regex::escape`]) — there is no implicit
//! regex interpretation.  Pass `--regex`/`-E` (CLI) or `"regex": true` (MCP)
//! to interpret `pattern` as a `regex::bytes` pattern instead.  This exists
//! because ambiguous capture-group syntax (see below) is easy to get wrong
//! by accident when regex parsing kicks in for what was meant to be a plain
//! literal search/replace.
//!
//! ## Capture-group expansion vs. literal `$`
//!
//! Capture-group references (`$0`, `$1`, `$name`, `$$`) are expanded by the
//! regex engine via [`regex::bytes::Captures::expand`] **only when the pattern
//! actually contains at least one explicit capture group**.  When the pattern
//! has no groups — every non-regex (literal) search, and any regex without
//! `( … )` — the replacement bytes are written verbatim, so a literal `$`
//! (prices like `$5.00`, shell variables, `${TOKEN}` placeholders) is
//! preserved instead of being silently consumed as a group reference.  This
//! means `$0` is *not* interpreted as "the whole match" for a group-less
//! pattern; add a capturing group (e.g. wrap the pattern in `( … )` and use
//! `$1`) if you need a back-reference.
//!
//! When you *do* opt into regex with capture groups, disambiguate a numbered
//! reference from following literal text with braces: `${1}token`, not
//! `$1token` — the latter is parsed as a reference to a group *named*
//! `1token`, which almost never exists, silently dropping both the group
//! substitution and the literal suffix.
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
/// Options for [`run`], bundling the many positional flags so that call
/// sites name each field and cannot accidentally transpose the bare
/// booleans (`multiline` / `regex` / `count_only` / `dry_run`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ReplaceOptions {
    /// Prepend `(?m)` to the pattern so `^` / `$` match at LF boundaries.
    pub multiline: bool,
    /// Interpret `pattern` as a `regex::bytes` pattern.  When `false` (the
    /// default), `pattern` is treated as a fixed literal string (every regex
    /// metacharacter is escaped) — regex is opt-in, never implicit.
    pub regex: bool,
    /// Override the output line ending; `None` preserves the file's.
    pub line_ending_override: Option<LineEnding>,
    /// Count matches without modifying the file.
    pub count_only: bool,
    /// Compute the substitution in memory without writing.
    pub dry_run: bool,
    /// File access strategy (mmap vs buffered).
    pub io_mode: IoMode,
    /// Write-time mojibake guard policy.
    pub policy: WritePolicy,
}

/// A single contiguous region changed by one match/replacement, used to
/// build a compact "changed region" echo without paying for a whole-file
/// diff.  Line numbers refer to the ORIGINAL (pre-edit) file; `new_text` is
/// the LF-normalised text that now replaces that span.
///
/// Deliberately cheap to produce: computed from data already resident in
/// memory for the substitution itself (the matched span and the expanded
/// replacement), never from a full-file clone or a whole-file diff.
#[derive(Debug, Clone)]
pub struct ChangedRegion {
    /// 1-based inclusive starting line of the matched span in the original file.
    pub start_line: usize,
    /// 1-based inclusive ending line of the matched span in the original file
    /// (equal to `start_line` for a single-line match).
    pub end_line: usize,
    /// Number of lines in `new_text` (0 for an empty replacement).  Always
    /// accurate regardless of [`RegionsRequest::text_budget_lines`], even
    /// when `new_text` itself was left empty to stay under budget.
    pub new_line_count: usize,
    /// LF-normalised replacement text for this region, or empty once
    /// [`RegionsRequest::text_budget_lines`] has been exhausted (see there).
    pub new_text: String,
}

/// Request to collect [`ChangedRegion`]s during [`run`], with an optional
/// memory bound on how much replacement text is retained.
pub struct RegionsRequest<'a> {
    /// Regions are appended here, one per match, in file order.
    pub regions_out: &'a mut Vec<ChangedRegion>,
    /// Once the running total of `new_line_count` already collected reaches
    /// this many lines, subsequent regions still report accurate
    /// `start_line`/`end_line`/`new_line_count` but leave `new_text` empty
    /// — this bounds retained text to roughly this many lines' worth
    /// rather than the full size of every match's replacement, which
    /// matters when there are many (or very large) matches whose combined
    /// echo will never actually be shown (see `tpu_replace_in_file`'s
    /// `echo_max_lines`). `None` means no limit: always materialise text.
    pub text_budget_lines: Option<usize>,
}

/// Apply a regex (or fixed-string) replacement to `file` in place.
///
/// All boolean and policy knobs are bundled into [`ReplaceOptions`]; see its
/// fields for the per-flag behaviour.  `diff_out`, when `Some`, receives a
/// unified text diff (in normalised/LF space) after a successful write.
///
/// `regions`, when `Some`, is populated with one [`ChangedRegion`] per
/// match — cheap to compute regardless of file size, so callers can build a
/// compact "changed region" echo without needing to opt into the full
/// whole-file diff (which clones the entire normalised file).
pub fn run(
    file: &Path,
    pattern: &str,
    replacement: &[u8],
    diff_out: Option<&mut dyn Write>,
    mut regions: Option<RegionsRequest<'_>>,
    opts: ReplaceOptions,
) -> Result<usize, Box<dyn std::error::Error>> {
    let ReplaceOptions {
        multiline,
        regex,
        line_ending_override,
        count_only,
        dry_run,
        io_mode,
        policy,
    } = opts;
    let escaped = if regex {
        pattern.to_owned()
    } else {
        regex::escape(pattern)
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

    // When the pattern has no explicit capture groups, `$` in the replacement
    // cannot reference anything useful, so we treat the replacement bytes as a
    // literal string rather than a capture-group template.  This keeps `$`
    // (e.g. prices like `$5.00`, shell variables, template placeholders)
    // intact instead of silently consuming it as a group reference.  Non-
    // regex (literal) patterns always land here, so a literal search implies
    // a literal replacement.  `captures_len()` counts the implicit
    // whole-match group 0, so `> 1` means at least one explicit group exists.
    let has_capture_groups = re.captures_len() > 1;

    // Snapshot the normalised old bytes now for diff computation later.
    // Only paid when --diff or --dry-run is requested: this is a full-file
    // clone, so it must stay opt-in (see ChangedRegion for the cheap,
    // always-available alternative used for the default changed-region echo).
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

    // Collect all matches as source-coordinate splices.  Matches are visited
    // in left-to-right order (regex::bytes::Captures::iter guarantees this),
    // so `line_no`/`scanned_to` track cumulative line position incrementally:
    // each match only counts newlines in the *unscanned* gap since the last
    // match, never rescanning from the start of the file. Total newline-
    // counting work across all matches is therefore a single linear pass
    // over the file, the same order as reading it once for matching.
    let mut line_no: usize = 1;
    let mut scanned_to: usize = 0;
    let mut text_materialized: usize = 0;
    let mut splices: Vec<Splice> = Vec::new();
    for caps in re.captures_iter(&view.bytes) {
        let m = caps.get(0).unwrap();
        let source_start = view.byte_range_start() + view.offset_map.to_source(m.start() as u64);
        let source_end = view.byte_range_start() + view.offset_map.to_source(m.end() as u64);
        let source_len = source_end - source_start;

        // Expand capture-group back-references in normalised space.  When
        // the pattern has no explicit groups the replacement is taken
        // verbatim, so `$` survives instead of being read as a reference.
        let mut norm_repl: Vec<u8> = Vec::new();
        if has_capture_groups {
            caps.expand(replacement, &mut norm_repl);
        } else {
            norm_repl.extend_from_slice(replacement);
        }

        if let Some(req) = regions.as_mut() {
            line_no += view.bytes[scanned_to..m.start()]
                .iter()
                .filter(|&&b| b == b'\n')
                .count();
            let start_line = line_no;
            // A newline that is the LAST byte of the match only terminates
            // the match's own last line -- it doesn't pull in any content
            // from the following line, so it must not extend end_line.
            let match_span = &view.bytes[m.start()..m.end()];
            let counted_span = match match_span.last() {
                Some(b'\n') => &match_span[..match_span.len() - 1],
                _ => match_span,
            };
            let lines_in_match = counted_span.iter().filter(|&&b| b == b'\n').count();
            let end_line = start_line + lines_in_match;
            // Mirrors render_changed_regions' line splitting: a trailing
            // '\n' terminates the last line rather than starting a new
            // (empty) one, so it must not be counted as an extra line.
            let new_line_count = if norm_repl.is_empty() {
                0
            } else {
                let newline_count = norm_repl.iter().filter(|&&b| b == b'\n').count();
                if norm_repl.last() == Some(&b'\n') {
                    newline_count
                } else {
                    newline_count + 1
                }
            };
            // Bound retained text to roughly `text_budget_lines` lines total
            // (see RegionsRequest doc) rather than the full size of every
            // match's replacement -- line-span numbers stay accurate either
            // way, only `new_text` is left empty once over budget.
            let under_budget = match req.text_budget_lines {
                None => true,
                Some(budget) => text_materialized < budget,
            };
            let new_text = if under_budget {
                text_materialized += new_line_count;
                String::from_utf8_lossy(&norm_repl).into_owned()
            } else {
                String::new()
            };
            req.regions_out.push(ChangedRegion {
                start_line,
                end_line,
                new_line_count,
                new_text,
            });
            // `line_no` must track the line number of `scanned_to`, which
            // physically advances past the match's trailing '\n' (if any)
            // even though that byte was excluded from `end_line` above --
            // otherwise the next match's gap-count would silently miss
            // that line-boundary crossing and under-count start_line.
            line_no = if match_span.last() == Some(&b'\n') {
                end_line + 1
            } else {
                end_line
            };
            scanned_to = m.end();
        }

        // Denormalise: restore the file's dominant line terminator.
        let content = denormalize_bytes(&norm_repl, line_ending);

        splices.push(Splice {
            source_start,
            source_len,
            content,
        });
    }

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
        Some(target) => crate::encoding::apply_line_ending_to_all(out_bytes, file_encoding, target),
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
        let new_norm = if has_capture_groups {
            re.replace_all(&old, replacement).into_owned()
        } else {
            re.replace_all(&old, regex::bytes::NoExpand(replacement))
                .into_owned()
        };
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
        regex: bool,
        line_ending_override: Option<LineEnding>,
        diff_out: Option<&mut dyn Write>,
        count: bool,
        dry_run: bool,
    ) -> Result<usize, Box<dyn std::error::Error>> {
        run(
            file,
            pattern,
            replacement,
            diff_out,
            None,
            ReplaceOptions {
                multiline,
                regex,
                line_ending_override,
                count_only: count,
                dry_run,
                io_mode: IoMode::Mmap,
                policy: crate::mojibake::WritePolicy::permissive(),
            },
        )
    }

    /// Write `content` to a temp file, run `replace::run`, and return the
    /// resulting file bytes.  `diff` output is captured and returned separately.
    ///
    /// Always exercises regex mode (`regex: true`) internally — most of this
    /// suite's patterns rely on regex features (anchors, groups, character
    /// classes). Tests that specifically need default *literal* matching
    /// call `run_test` directly with `regex: false` instead.
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
            true,
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

    /// Write `content` to a temp file, run `replace::run` with regions
    /// collection enabled, and return the collected [`ChangedRegion`]s
    /// (file content itself is not needed by these tests).
    fn replace_file_regions(
        content: &[u8],
        pattern: &str,
        replacement: &[u8],
        regex: bool,
        text_budget_lines: Option<usize>,
    ) -> Vec<ChangedRegion> {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        let path = f.path().to_path_buf();
        drop(f);
        fs::write(&path, content).unwrap();

        let mut regions: Vec<ChangedRegion> = Vec::new();
        run(
            &path,
            pattern,
            replacement,
            None,
            Some(RegionsRequest {
                regions_out: &mut regions,
                text_budget_lines,
            }),
            ReplaceOptions {
                regex,
                io_mode: IoMode::Mmap,
                policy: crate::mojibake::WritePolicy::permissive(),
                ..Default::default()
            },
        )
        .unwrap();

        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&bak);
        let _ = fs::remove_file(&path);
        regions
    }

    // ── ChangedRegion (cheap changed-region echo, no full-file diff) ─────────

    /// A single-line match/replacement produces one region spanning that
    /// line, with the replacement text and its line count.
    #[test]
    fn changed_region_single_line_match() {
        let regions = replace_file_regions(
            b"hello world\nsecond line\n",
            "world",
            b"there",
            false,
            None,
        );
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start_line, 1);
        assert_eq!(regions[0].end_line, 1);
        assert_eq!(regions[0].new_line_count, 1);
        assert_eq!(regions[0].new_text, "there");
    }

    /// A match spanning multiple original lines reports the correct
    /// start/end line range, and a multi-line replacement reports the
    /// correct new_line_count.
    #[test]
    fn changed_region_multiline_match_and_replacement() {
        let content = b"one\ntwo\nthree\nfour\n";
        // Regex spans lines 2-3 ("two\nthree"); multiline dot-all not needed
        // since the literal pattern itself contains the newline.
        let regions = replace_file_regions(content, "two\nthree", b"TWO\nTHREE\nEXTRA", true, None);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start_line, 2);
        assert_eq!(regions[0].end_line, 3);
        assert_eq!(regions[0].new_line_count, 3);
        assert_eq!(regions[0].new_text, "TWO\nTHREE\nEXTRA");
    }

    /// Regression for issue review feedback: a replacement ending in a
    /// trailing `\n` must not be over-counted as an extra empty line --
    /// `new_line_count` must match how `render_changed_regions` actually
    /// splits/renders `new_text` (which drops the spurious trailing empty
    /// element produced by `str::split('\n')` on a trailing separator).
    #[test]
    fn changed_region_replacement_with_trailing_newline() {
        let regions = replace_file_regions(
            b"hello world\n",
            "world",
            b"line one\nline two\n",
            false,
            None,
        );
        assert_eq!(regions.len(), 1);
        assert_eq!(
            regions[0].new_line_count, 2,
            "trailing \\n must not count as a third (empty) line"
        );
        assert_eq!(regions[0].new_text, "line one\nline two\n");
    }

    /// Two separate matches later in the file report correctly
    /// incrementing line numbers (regression for the cumulative,
    /// single-pass line-counting logic: the second match's start_line must
    /// not be computed as if it were still at the start of the file).
    #[test]
    fn changed_region_multiple_matches_cumulative_line_numbers() {
        let content = b"a\nfoo\nb\nfoo\nc\n";
        let regions = replace_file_regions(content, "foo", b"bar", false, None);
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].start_line, 2);
        assert_eq!(regions[0].end_line, 2);
        assert_eq!(regions[1].start_line, 4);
        assert_eq!(regions[1].end_line, 4);
    }

    /// An empty replacement (pure deletion) reports `new_line_count: 0` and
    /// empty `new_text`, while still reporting the correct old line span.
    /// Regression for the trailing-newline boundary case: a match ending
    /// exactly on a line terminator (`"delete me\n"`) must report
    /// `end_line == start_line`, not `start_line + 1` -- the terminator
    /// ends the matched line, it doesn't pull in the next line's content.
    #[test]
    fn changed_region_empty_replacement_is_deletion() {
        let regions =
            replace_file_regions(b"keep\ndelete me\nkeep2\n", "delete me\n", b"", false, None);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start_line, 2);
        assert_eq!(regions[0].end_line, 2);
        assert_eq!(regions[0].new_line_count, 0);
        assert_eq!(regions[0].new_text, "");
    }

    /// `text_budget_lines` bounds retained `new_text` without affecting the
    /// accuracy of `new_line_count`/`start_line`/`end_line`: once the
    /// running total of materialised lines reaches the budget, later
    /// regions still report correct sizes but leave `new_text` empty.
    #[test]
    fn changed_region_text_budget_bounds_retained_text() {
        let content = b"a\nfoo\nb\nfoo\nc\nfoo\nd\n";
        // Three matches, each a 1-line replacement; budget covers only the
        // first match's line.
        let regions = replace_file_regions(content, "foo", b"bar", false, Some(1));
        assert_eq!(regions.len(), 3);
        // First region is under budget (0 < 1): text materialised.
        assert_eq!(regions[0].new_text, "bar");
        assert_eq!(regions[0].new_line_count, 1);
        // Second and third regions are over budget: sizes stay accurate,
        // text is left empty.
        assert_eq!(regions[1].new_text, "");
        assert_eq!(regions[1].new_line_count, 1);
        assert_eq!(regions[1].start_line, 4);
        assert_eq!(regions[2].new_text, "");
        assert_eq!(regions[2].new_line_count, 1);
        assert_eq!(regions[2].start_line, 6);
    }

    /// Regression: `line_no` must track the line number of `scanned_to`,
    /// not just `end_line` -- a match ending in a trailing `\n` physically
    /// advances `scanned_to` past that newline even though it's excluded
    /// from `end_line` (see the deletion test above), so a subsequent
    /// match's `start_line` would be under-counted by one line if `line_no`
    /// weren't also advanced past it.
    #[test]
    fn changed_region_match_trailing_newline_does_not_undercount_next_match() {
        let content = b"keep\ndelete me\nkeep2\nfoo\nkeep3\n";
        let regions = replace_file_regions(content, "delete me\n|foo", b"X", true, None);
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].start_line, 2);
        assert_eq!(regions[0].end_line, 2);
        assert_eq!(
            regions[1].start_line, 4,
            "second match must not be under-counted by the first match's trailing newline"
        );
        assert_eq!(regions[1].end_line, 4);
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

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.txt");
        fs::write(&path, &bytes).unwrap();

        run_test(
            &path,
            pattern,
            replacement,
            false,
            true,
            line_ending_override,
            None,
            false,
            false,
        )
        .unwrap();

        fs::read(&path).unwrap()
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
        // $0 expands to the whole match when the pattern has a capturing group.
        let (out, _) = replace_file(b"hello\n", "(ell)", b"[$0]", false, false);
        assert_eq!(out, b"h[ell]o\n");
    }

    #[test]
    fn replace_literal_dollar() {
        // $$ in the replacement template becomes a literal $ when the pattern
        // has a capturing group (so capture expansion is active).
        let (out, _) = replace_file(b"price 10\n", r"(\d+)", b"$$$$5", false, false);
        assert_eq!(out, b"price $$5\n");
    }

    #[test]
    fn replace_group_less_pattern_keeps_dollar_literal() {
        // With no capturing group the replacement is literal, so a bare `$`
        // (prices, variables, placeholders) survives instead of being read as
        // a capture reference.
        let (out, _) = replace_file(b"amount X\n", "X", b"$5.00", false, false);
        assert_eq!(out, b"amount $5.00\n");
    }

    #[test]
    fn replace_group_less_pattern_dollar_zero_is_literal() {
        // $0 is NOT interpreted as the whole match when the pattern has no
        // capturing group — it is written verbatim.
        let (out, _) = replace_file(b"hello\n", "ell", b"[$0]", false, false);
        assert_eq!(out, b"h[$0]o\n");
    }

    #[test]
    fn replace_literal_pattern_keeps_dollar_literal() {
        // A literal (non-regex) search never has a capturing group, so its
        // replacement is always literal.
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"COST here\n").unwrap();
        f.flush().unwrap();
        let path = f.path().to_path_buf();
        drop(f);
        fs::write(&path, b"COST here\n").unwrap();
        run_test(
            &path, "COST", b"$9.99", false, false, None, None, false, false,
        )
        .unwrap();
        let out = fs::read(&path).unwrap();
        let _ = fs::remove_file(format!("{}.bak", path.display()));
        let _ = fs::remove_file(&path);
        assert_eq!(out, b"$9.99 here\n");
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
            true,
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
            true,
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
