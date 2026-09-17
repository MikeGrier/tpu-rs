// Copyright (c) 2026, Michael Grier

//! `tpu replace` — in-place regex replace on an encoding-aware, normalised
//! view of a file.
//!
//! The pattern is applied to a LF-only normalised view so callers never need
//! to account for CRLF in their patterns.  `\n` in patterns always matches
//! the LF byte used inside the normalised view. Replacements are denormalised
//! to explicit or Git policy, or the file's dominant line ending. The result is
//! written atomically via a temp file; the original is renamed to `<file>.bak`.
//!
//! `--multiline` prepends `(?m)` to the pattern, making `^` / `$` match at
//! LF boundaries within the file rather than only at the start / end of the
//! entire content.
//!
//! `--diff` writes a unified text diff of the changes (in normalised/LF space)
//! to the provided writer after the file has been successfully updated.
//!
//! ## Zero-match short-circuit
//!
//! When the pattern matches zero times, no `line_ending_override` is set,
//! and the caller is not asking for a count-only or dry-run preview,
//! [`run`] returns a [`ReplaceOutcome`] whose `total()` is 0 and whose
//! `wrote`/`would_write` are false, without rewriting the file: no
//! `materialize`, no atomic rewrite, no `<file>.bak`, no mtime bump.  (The
//! file is still read and decoded -- that is how the count is known -- so
//! this is a write short-circuit, not a read one.)  This makes an unmatched
//! pattern observably distinct from a real edit at the file-system level,
//! and avoids one wasted full-file rewrite per call whose result would have
//! been byte-identical.
//!
//! When `line_ending_override` is set, the short-circuit is skipped: the
//! override is itself a real change to the file even with zero
//! substitutions (CRLF -> LF etc.), so the normal write path runs.
//!
//! ## Replacement-string escapes
//!
//! [`run`] takes the replacement as raw `&[u8]` for signature convenience
//! (matching [`decode_replacement`]'s `Vec<u8>` output), but the bytes
//! **must be valid UTF-8** once escape decoding is complete: `replace`
//! operates on the file's decoded UTF-8 text throughout (matching/expanding
//! against `old_norm`, which is `old_text.as_bytes()` of an already-decoded
//! `&str`) and re-encodes strictly to the file's target encoding afterward,
//! so a non-UTF-8 replacement has nothing valid to expand into. [`run`]
//! checks this explicitly up front and returns a `"replacement is not valid
//! UTF-8"` error rather than silently corrupting the match expansion.  In
//! practice this only bites a `\xHH` escape (or a raw byte from
//! `--literal-replacement`) that doesn't form a valid UTF-8 sequence on its
//! own — e.g. `\xFF` alone; multi-byte sequences like `\xC3\xA9` (é) are
//! fine.  Backslash-escape decoding (`\n` → LF, `\t` → TAB, `\\` → `\`,
//! `\xHH`, `\uXXXX`, …) is the responsibility of the *caller* — the CLI
//! front-end in `main.rs` performs that decoding via
//! [`crate::escape::decode_bytes`] unless the user passes
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

use std::{io::Write, path::Path};

use harrier::encoding::LineEnding;
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
/// Either way the result is LF-normalised. Substitution happens in LF-only
/// space and the target convention is applied once at write time, so a CR
/// arriving here — from a `\r` escape or a literal one — would survive into
/// [`crate::encoding::denormalize_lf_to_crlf`], whose precondition it breaks,
/// and land on disk as `\r\r\n`.
///
/// Errors are returned as `String` so the CLI can surface them directly with
/// a `replace:` prefix.
pub fn decode_replacement(s: &str, literal: bool) -> Result<Vec<u8>, String> {
    let bytes = if literal {
        s.as_bytes().to_vec()
    } else {
        crate::escape::decode_bytes(s).map_err(|e| format!("invalid escape in replacement: {e}"))?
    };
    Ok(crate::encoding::normalize_bytes_to_lf(&bytes))
}

/// Options for [`run`] and [`run_batch`], bundling the many positional flags
/// so that call sites name each field and cannot accidentally transpose the
/// bare booleans (`multiline` / `regex` / `count_only` / `dry_run`).
///
/// `multiline` prepends `(?m)` so `^` / `$` match at every LF boundary; `\n`
/// in a pattern always refers to the LF byte used internally, so CRLF is
/// transparent. `line_ending_override` replaces Git policy (and the file's
/// detected dominant ending) as the denormalisation target; content encoding
/// still follows Git policy or is preserved. `count_only` and `dry_run` write
/// nothing. `policy` controls the write-time mojibake guard, which rejects
/// content introducing mojibake not present in the file's prior decoded
/// content; pass [`WritePolicy::permissive`] (or the CLI's
/// `--allow-mojibake`) to skip it.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReplaceOptions {
    /// Prepend `(?m)` to the pattern so `^` / `$` match at LF boundaries.
    pub multiline: bool,
    /// Interpret `pattern` as a `regex::bytes` pattern.  When `false` (the
    /// default), `pattern` is treated as a fixed literal string (every regex
    /// metacharacter is escaped) — regex is opt-in, never implicit.
    pub regex: bool,
    /// Override the output line ending; `None` uses Git policy or preserves the file's.
    pub line_ending_override: Option<LineEnding>,
    /// Count matches without modifying the file.
    pub count_only: bool,
    /// Compute the substitution in memory without writing.
    pub dry_run: bool,
    /// File access strategy (mmap vs buffered).
    pub io_mode: IoMode,
    /// Write-time mojibake guard policy.
    pub policy: WritePolicy,
    /// Collect a before/after image of every changed line.
    ///
    /// Off by default because it costs a whole-file line diff, which the
    /// cheap [`ChangedRegion`] echo exists to avoid.
    pub changed_lines: bool,
    /// Stop collecting after this many changed lines, setting
    /// [`ReplaceOutcome::changed_lines_truncated`]. `None` means no bound.
    pub changed_lines_max: Option<usize>,
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
    /// Once materialising a region's `new_text` would push the running
    /// total of `new_line_count` already collected past this many lines,
    /// that region (and every later one) still reports accurate
    /// `start_line`/`end_line`/`new_line_count` but leaves `new_text` empty
    /// — this bounds retained text to at most this many lines' worth
    /// rather than the full size of every match's replacement, which
    /// matters when there are many (or very large) matches whose combined
    /// echo will never actually be shown (see `tpu_replace_in_file`'s
    /// `echo_max_lines`). The check accounts for the candidate region's own
    /// size, not just the running total, so a single match far larger than
    /// the budget is never fully materialised. `None` means no limit:
    /// always materialise text.
    pub text_budget_lines: Option<usize>,
}

/// Everything a caller needs to verify that a replace did what it expected.
///
/// A bare substitution count answers "did anything happen" but not "did the
/// right thing happen, and is the file still committable". The line-ending
/// census answers the second question directly: a whole-file replace always
/// rewrites every terminator to one convention, so a mixed file is silently
/// homogenised by an edit that had nothing to do with line endings. Reporting
/// `before` and `after` makes that visible instead of leaving it to be
/// discovered at `git commit`.
#[derive(Debug, Clone, Default)]
pub struct ReplaceOutcome {
    /// One match count per op, positionally aligned with the request.
    pub counts: Vec<usize>,
    /// Terminator census of the file as it was read.
    pub before: crate::TextLayout,
    /// Terminator census of the file as written — or as it *would* be written
    /// under `dry_run` / `count_only`. Equal to `before` when no write
    /// happened.
    pub after: crate::TextLayout,
    /// Per-line before/after images, when [`ReplaceOptions::changed_lines`]
    /// asked for them.
    pub changed_lines: Vec<ChangedLine>,
    /// Whether `changed_lines` was truncated by
    /// [`ReplaceOptions::changed_lines_max`].
    pub changed_lines_truncated: bool,
    /// Whether bytes actually reached the disk.
    ///
    /// Distinct from a non-zero count: an identity substitution matches but
    /// produces byte-identical output, which the write path skips. Callers
    /// that stamp mtime or clear a `.bak` on "something happened" must key off
    /// this, not the count, or a no-op mutates the file's metadata.
    pub wrote: bool,
    /// Whether the resulting bytes differ from what is on disk.
    ///
    /// Equal to [`Self::wrote`] for a real run; under `dry_run` it is the
    /// answer `wrote` cannot give. Callers deriving "would anything change"
    /// must use this rather than an empty diff: the diff is computed in
    /// LF-normalised space, where a pure line-ending rewrite looks identical.
    pub would_write: bool,
}

impl ReplaceOutcome {
    /// Total substitutions across every op.
    pub fn total(&self) -> usize {
        self.counts.iter().sum()
    }

    /// True when the write collapsed a mixed file onto one convention.
    ///
    /// Not an error — it is what a whole-file rewrite necessarily does — but
    /// it is a change the caller did not ask for and should know about.
    /// Requires an actually uniform result: a replace that strips every
    /// terminator leaves `none`, which is not a convention to have normalised
    /// onto.
    pub fn normalized_line_endings(&self) -> bool {
        self.before.is_mixed() && self.after.uniformity() == "uniform"
    }
}

/// One line that differs between the file as read and the file as written.
///
/// Derived from a line-level diff of the final before/after text, so the
/// positions are meaningful for a batch too: they name real lines of the real
/// files, not of some intermediate buffer a single op happened to see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedLine {
    /// 1-based line number in the file as read; `None` for an inserted line.
    pub old_line: Option<usize>,
    /// 1-based line number in the file as written; `None` for a deleted line.
    pub new_line: Option<usize>,
    /// The line's content before, without its terminator.
    pub old_text: Option<String>,
    /// The line's content after, without its terminator.
    pub new_text: Option<String>,
}

impl ChangedLine {
    #[allow(dead_code)] // Used by tpu-mcp (library consumer), not by the tpu binary.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "old_line": self.old_line,
            "new_line": self.new_line,
            "old_text": self.old_text,
            "new_text": self.new_text,
        })
    }
}

/// Apply a regex (or fixed-string) replacement to `file` in place.
///
/// Returns a [`ReplaceOutcome`]: `total()` is the substitution count, `wrote`
/// and `would_write` say whether bytes reached (or would reach) the disk, and
/// `before`/`after` carry the line-ending census. A non-zero count does NOT
/// imply a write — an identity substitution matches and produces
/// byte-identical output, which the write path skips.
///
/// All boolean and policy knobs are bundled into [`ReplaceOptions`]; see its
/// fields for the per-flag behaviour.  `diff_out`, when `Some`, receives a
/// unified text diff (in normalised/LF space) after a successful write.
///
/// `regions`, when `Some`, is populated with one [`ChangedRegion`] per
/// match — cheap to compute regardless of file size, so callers can build a
/// compact "changed region" echo without needing to opt into the full
/// whole-file diff (which clones the entire normalised file). Note these
/// describe *matches*, not bytes written, so a caller rendering them should
/// check `wrote` first.
pub fn run(
    file: &Path,
    pattern: &str,
    replacement: &[u8],
    diff_out: Option<&mut dyn Write>,
    regions: Option<RegionsRequest<'_>>,
    opts: ReplaceOptions,
) -> Result<ReplaceOutcome, Box<dyn std::error::Error>> {
    let op = ReplaceOp {
        label: None,
        pattern,
        replacement,
        regex: opts.regex,
        multiline: opts.multiline,
        // The single-op path has always returned `Ok(0)` for a pattern that
        // matched nothing and left the refusal to its callers; only a batch
        // needs the all-or-nothing guarantee baked in here.
        allow_no_match: true,
    };
    apply(file, std::slice::from_ref(&op), diff_out, regions, opts)
}

/// One substitution within a batch.
///
/// `regex` and `multiline` are per-op rather than per-call so a single batch
/// can mix a literal rename with an anchored regex rewrite.
pub struct ReplaceOp<'a> {
    /// Caller-supplied name for this op, echoed back beside its count so a
    /// batch's result reads as a tally rather than an anonymous list.
    pub label: Option<&'a str>,
    pub pattern: &'a str,
    pub replacement: &'a [u8],
    /// Interpret `pattern` as a `regex::bytes` pattern instead of a literal.
    pub regex: bool,
    /// Prepend `(?m)` so `^` / `$` match at LF boundaries.
    pub multiline: bool,
    /// When `false`, this op matching nothing refuses the **whole** batch.
    pub allow_no_match: bool,
}

/// Apply several substitutions to `file` in one atomic write.
///
/// Ops run **in order against the evolving buffer**, so a later op sees the
/// output of every earlier one — the same semantics as a chain of
/// `String::replace` calls, which is what this exists to replace. Returns one
/// match count per op, positionally aligned with `ops`.
///
/// The whole batch is one write: one temp-file swap, one `<file>.bak`, one
/// mtime bump, one mojibake-guard check against the file's original content —
/// and only when the resulting bytes differ, which an identity substitution's
/// do not (see [`ReplaceOutcome::wrote`]).
/// A batch that fails partway therefore cannot leave a half-transformed file,
/// which is the failure mode of running the ops as N separate calls.
///
/// Any op whose count is zero and whose `allow_no_match` is `false` refuses
/// the entire batch and leaves the file untouched — a mis-anchored pattern in
/// the middle of a rename is exactly the mistake a silent no-op hides, and
/// unlike the single-op path there is no caller left to notice it per-op.
///
/// [`ChangedRegion`] echoes are deliberately not offered here: a region's line
/// numbers refer to the buffer as that op saw it, and once an earlier op has
/// added or removed lines those numbers describe neither the original file nor
/// the final one. Pass `diff_out` for an unambiguous whole-file old/new diff,
/// or read the per-op counts.
pub fn run_batch(
    file: &Path,
    ops: &[ReplaceOp<'_>],
    diff_out: Option<&mut dyn Write>,
    opts: ReplaceOptions,
) -> Result<ReplaceOutcome, Box<dyn std::error::Error>> {
    // Both are per-op in batch mode. Honouring the struct field would apply it
    // to every op; ignoring it silently gives literal, non-multiline matching
    // to a caller who asked for the opposite.
    if opts.regex || opts.multiline {
        return Err(format!(
            "replace: {}: 'regex' and 'multiline' are per-op in batch mode; set them on \
             each ops entry rather than on ReplaceOptions",
            file.display()
        )
        .into());
    }
    apply(file, ops, diff_out, None, opts)
}

// ──────────────────────────────────────────────────────────────────────────────
// The shared `ops` schema
// ──────────────────────────────────────────────────────────────────────────────
//
// `tpu_replace_in_file`'s `ops` argument and `tpu replace --ops FILE` are the
// same JSON array of the same objects, parsed here by the same code. Keeping
// one decoder is the point: a batch written for one front end is copy-pasteable
// into the other, and neither can grow a field the other silently ignores.

/// An owned [`ReplaceOp`], as decoded from the shared JSON object form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedReplaceOp {
    pub label: Option<String>,
    pub pattern: String,
    pub replacement: String,
    pub regex: bool,
    pub multiline: bool,
    pub allow_no_match: bool,
}

impl OwnedReplaceOp {
    pub fn as_op(&self) -> ReplaceOp<'_> {
        ReplaceOp {
            label: self.label.as_deref(),
            pattern: &self.pattern,
            replacement: self.replacement.as_bytes(),
            regex: self.regex,
            multiline: self.multiline,
            allow_no_match: self.allow_no_match,
        }
    }
}

/// Borrow a whole slice of owned ops for [`run_batch`].
pub fn as_ops(owned: &[OwnedReplaceOp]) -> Vec<ReplaceOp<'_>> {
    owned.iter().map(OwnedReplaceOp::as_op).collect()
}

/// Decode an `ops` array: a JSON array of substitution objects.
pub fn ops_from_json(value: &serde_json::Value) -> Result<Vec<OwnedReplaceOp>, String> {
    let entries = value
        .as_array()
        .ok_or("'ops' must be a JSON array of substitution objects")?;
    if entries.is_empty() {
        return Err("'ops' must contain at least one substitution".into());
    }
    entries
        .iter()
        .enumerate()
        .map(|(i, entry)| op_from_json(entry).map_err(|e| format!("ops[{i}]: {e}")))
        .collect()
}

/// Read the `ops` array from a file (or stdin when `path` is `-`).
pub fn ops_from_file(path: &Path) -> Result<Vec<OwnedReplaceOp>, String> {
    let text = if path == Path::new("-") {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .map_err(|e| format!("--ops: reading stdin: {e}"))?;
        buf
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("--ops {}: {e}", path.display()))?
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("--ops {}: not valid JSON: {e}", path.display()))?;
    ops_from_json(&value).map_err(|e| format!("--ops {}: {e}", path.display()))
}

/// Decode one substitution object.
///
/// `replacement` is taken **verbatim** unless the object sets
/// `expand_escapes: true` — the same rule as every other tpu text payload, and
/// deliberately *not* the CLI positional `REPLACEMENT`'s escape-decoding
/// default, which exists only to match `sed`/`perl` habits at a shell prompt.
fn op_from_json(entry: &serde_json::Value) -> Result<OwnedReplaceOp, String> {
    if !entry.is_object() {
        return Err(format!("must be a JSON object, got {entry}"));
    }
    for key in entry.as_object().into_iter().flat_map(|o| o.keys()) {
        if !OP_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "unknown field {key:?}; expected one of {}",
                OP_KEYS.join(", ")
            ));
        }
    }
    let expand_escapes = op_bool(entry, "expand_escapes")?;
    let replacement_format = op_format(entry, "replacement_format")?;
    if expand_escapes && replacement_format.is_some() {
        return Err(
            "'expand_escapes' cannot be combined with 'replacement_format': the \
                    encoded payload already specifies the exact bytes to write"
                .into(),
        );
    }
    let pattern = match op_format(entry, "pattern_format")? {
        Some(fmt) => decode_utf8_payload(&fmt, op_str(entry, "pattern")?, "pattern_format")?,
        None => op_str(entry, "pattern")?.to_owned(),
    };
    let regex = op_bool(entry, "regex")?;
    // An empty literal pattern matches at every byte position, so it splices
    // the replacement between every character of the file -- with a large,
    // plausible-looking count that the all-or-nothing guard never questions.
    // Almost always a template variable that resolved to nothing.
    if pattern.is_empty() && !regex {
        return Err(
            "'pattern' is empty, which would match at every position and rewrite \
                    the whole file; pass regex:true if an empty pattern is genuinely \
                    intended"
                .into(),
        );
    }
    let replacement = match replacement_format {
        Some(fmt) => {
            decode_utf8_payload(&fmt, op_str(entry, "replacement")?, "replacement_format")?
        }
        None if expand_escapes => unescape_replacement(op_str(entry, "replacement")?),
        None => op_str(entry, "replacement")?.to_owned(),
    };
    Ok(OwnedReplaceOp {
        label: op_label(entry)?,
        // Both are normalised at the compile/write chokepoints in `apply`, so
        // neither needs it here; the replacement is normalised early only
        // because it is also compared for the escape-hazard guard.
        pattern,
        replacement: crate::encoding::normalize_to_lf(&replacement).into_owned(),
        regex,
        multiline: op_bool(entry, "multiline")?,
        allow_no_match: op_bool(entry, "allow_no_match")?,
    })
}

/// A mistyped `label` is an error, not a silent `None`: the per-op tally is
/// what a caller is told to verify against, and an anonymous row defeats that.
fn op_label(entry: &serde_json::Value) -> Result<Option<String>, String> {
    match entry.get("label") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("'label' must be a JSON string, got {other}")),
    }
}

const OP_KEYS: &[&str] = &[
    "label",
    "pattern",
    "pattern_format",
    "replacement",
    "replacement_format",
    "expand_escapes",
    "regex",
    "multiline",
    "allow_no_match",
];

fn decode_utf8_payload(
    format: &crate::data_format::DataFormat,
    raw: &str,
    what: &str,
) -> Result<String, String> {
    let bytes = crate::data_format::decode(format, raw).map_err(|e| format!("{what}: {e}"))?;
    String::from_utf8(bytes).map_err(|e| format!("{what}: decoded bytes are not valid UTF-8: {e}"))
}

fn op_str<'a>(entry: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    entry
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing required string field '{key}'"))
}

/// A present-but-non-boolean value is an error, never a silent `false`: a
/// caller who sent `"true"` deserves to be told the flag did not take effect.
fn op_bool(entry: &serde_json::Value, key: &str) -> Result<bool, String> {
    match entry.get(key) {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        Some(other) => Err(format!("'{key}' must be a JSON boolean, got {other}")),
    }
}

fn op_format(
    entry: &serde_json::Value,
    key: &str,
) -> Result<Option<crate::data_format::DataFormat>, String> {
    match entry.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => {
            crate::data_format::DataFormat::from_name(s).map(Some)
        }
        Some(other) => Err(format!(
            "'{key}' must be a JSON string naming a data format, got {other}"
        )),
    }
}

/// Expand `\n`, `\r`, `\t`, and `\\` in a replacement template.
///
/// **Opt-in only** (`expand_escapes: true`); the default is verbatim. All
/// other `\X` sequences pass through with the backslash intact so that `$1`,
/// `$name`, and `$$` reach the regex engine unaltered.
#[allow(dead_code)] // Also used by tpu-mcp (library consumer).
pub fn unescape_replacement(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Shared implementation of [`run`] and [`run_batch`].
///
/// `regions`, when requested, describes the first op only — it is reachable
/// solely from the single-op [`run`] path.
fn apply(
    file: &Path,
    ops: &[ReplaceOp<'_>],
    diff_out: Option<&mut dyn Write>,
    mut regions: Option<RegionsRequest<'_>>,
    opts: ReplaceOptions,
) -> Result<ReplaceOutcome, Box<dyn std::error::Error>> {
    let ReplaceOptions {
        multiline: _,
        regex: _,
        line_ending_override,
        count_only,
        dry_run,
        io_mode,
        policy,
        changed_lines: want_changed_lines,
        changed_lines_max,
    } = opts;
    if ops.is_empty() {
        return Err(format!("replace: {}: no operations supplied", file.display()).into());
    }

    // Compile and validate every op before touching the file, so a bad pattern
    // or a non-UTF-8 replacement in op 5 cannot be discovered after ops 1..4
    // have already been applied to the buffer.
    let mut compiled: Vec<(Regex, bool, Vec<u8>)> = Vec::with_capacity(ops.len());
    for op in ops {
        // Guarded here rather than in the decoders so every front end is
        // covered: an empty literal pattern matches at every byte position and
        // splices the replacement between every character of the file.
        if op.pattern.is_empty() && !op.regex {
            return Err(format!(
                "replace: {}: 'pattern' is empty, which would match at every position \
                 and rewrite the whole file; pass regex:true if an empty pattern is \
                 genuinely intended",
                file.display()
            )
            .into());
        }
        // Matching happens against the file's LF-normalised view, so a literal
        // CR in the pattern could never match anything.
        let pattern = crate::encoding::normalize_to_lf(op.pattern);
        let escaped = if op.regex {
            pattern.into_owned()
        } else {
            regex::escape(&pattern)
        };
        let effective_pattern = if op.multiline {
            format!("(?m){escaped}")
        } else {
            escaped
        };
        let re = Regex::new(&effective_pattern)?;
        // `captures_len()` counts the implicit whole-match group 0, so `> 1`
        // means at least one explicit group exists.
        let has_capture_groups = re.captures_len() > 1;
        let replacement = std::str::from_utf8(op.replacement).map_err(|error| {
            format!(
                "replace: {}: replacement is not valid UTF-8: {error}",
                file.display()
            )
        })?;
        // The front ends normalise too, but `denormalize_lf_to_*` assumes an
        // LF-only buffer: a CR reaching the substitution is re-expanded into
        // `\r\r\n` for a CRLF target.
        let replacement = crate::encoding::normalize_to_lf(replacement)
            .into_owned()
            .into_bytes();
        compiled.push((re, has_capture_groups, replacement));
    }

    let decoded = crate::read_text_file(file, io_mode)?;
    let file_encoding = decoded.encoding;
    let detected_line_ending = decoded.line_ending;
    let had_bom = decoded.bom_len > 0;
    // The terminator census taken during the decode above — free, and taken
    // in Unicode space so UTF-16's interleaved CR/LF code units are counted
    // correctly rather than as stray bytes.
    let before = decoded.layout;
    let old_text = decoded.text;
    let old_norm = old_text.as_bytes();

    let mut counts: Vec<usize> = Vec::with_capacity(ops.len());
    let mut buf: std::borrow::Cow<'_, [u8]> = std::borrow::Cow::Borrowed(old_norm);
    for (index, (re, has_capture_groups, replacement)) in compiled.iter().enumerate() {
        let replacement = replacement.as_slice();
        let has_capture_groups = *has_capture_groups;
        let count = if index == 0 && regions.is_some() {
            collect_regions(re, replacement, has_capture_groups, &buf, &mut regions)
        } else {
            re.find_iter(&buf).count()
        };
        counts.push(count);
        if count > 0 {
            let next = if has_capture_groups {
                re.replace_all(&buf, replacement).into_owned()
            } else {
                re.replace_all(&buf, regex::bytes::NoExpand(replacement))
                    .into_owned()
            };
            buf = std::borrow::Cow::Owned(next);
        }
    }

    // Resolved once, before the preview branch, so `count`/`dry_run` predict
    // the write exactly -- including failing on a policy the write would fail
    // on, rather than silently reporting a census derived from a different
    // target.
    let git_policy = crate::git::policy_for_path(file)?;
    let replacement_count: usize = counts.iter().sum();

    // A batch is all-or-nothing: an op that matched nothing is a mis-anchored
    // pattern, and unlike the single-op path there is no per-call result left
    // for the caller to notice it in. Checked before any write, so the file is
    // untouched.
    //
    // Only introspection modes are exempt, because they write nothing. A
    // `line_ending_override` deliberately is NOT: it exempts the single-op
    // path (where zero matches would otherwise report failure for a call that
    // still rewrites the file), but a batch promises that a typo cannot
    // produce a partial rewrite, and whether the terminators are being
    // converted says nothing about whether the patterns were right. Set
    // `allow_no_match` on the op that is legitimately idempotent instead.
    if !count_only && !dry_run {
        for (op, count) in ops.iter().zip(&counts) {
            if *count == 0 && !op.allow_no_match {
                let label = op.label.map(|l| format!("{l}: ")).unwrap_or_default();
                return Err(format!(
                    "replace: {}: {label}pattern {:?} matched 0 times; the whole batch was \
                     refused and the file was not modified. Set allow_no_match on this op if \
                     a zero match is the intended outcome.",
                    file.display(),
                    op.pattern,
                )
                .into());
            }
        }
    }

    // Everything below is a pure function of the substituted text, so every
    // mode runs it: a preview that skipped these would report success for a
    // write that is going to fail, which is the opposite of what a preview is
    // for. None of it touches the disk.
    let new_norm = buf.into_owned();
    let new_text = std::str::from_utf8(&new_norm)
        .map_err(|error| format!("replace: generated invalid UTF-8: {error}"))?;
    let encoded = crate::encoding::encode_text_strict(new_text, file_encoding)
        .map_err(|error| format!("replace: {}: {error}", file.display()))?;
    if policy.reject_introduced_mojibake {
        check_write_does_not_introduce_mojibake(&old_text, new_text)
            .map_err(|e| format!("replace: {}: {e}", file.display()))?;
    }

    // A whole-file write denormalises every terminator to one convention, so
    // the resulting census is the substituted text's line structure projected
    // onto that target — no re-scan of the encoded output needed. This is also
    // why a replace silently homogenises a mixed file.
    //
    // A run that substitutes nothing and has no override rewrites nothing, so
    // its census is still `before`; claiming a projected one would predict a
    // write that never happens.
    let rewrites = replacement_count > 0 || line_ending_override.is_some();
    let target_line_ending = line_ending_override
        .or_else(|| git_policy.line_ending_for_text(new_text))
        .unwrap_or(detected_line_ending);
    let after = if rewrites {
        crate::TextLayout::analyze(new_text).projected_onto(target_line_ending)
    } else {
        before
    };

    // Zero-match short-circuit: no replacements means the atomic rewrite would
    // produce byte-identical content, so skip it entirely -- no materialize,
    // no atomic_write, no .bak, no mtime bump. Callers can then distinguish
    // "matched nothing" from "replaced N" at the file-system level, not just
    // via the returned count.
    //
    // Preserved side effect: a caller-supplied `line_ending_override` is
    // itself a real change to the file even with zero substitutions
    // (CRLF -> LF etc.), so skip the short-circuit in that case and fall
    // through to the normal write path.
    if !rewrites {
        return Ok(ReplaceOutcome {
            counts,
            before,
            after: before,
            ..Default::default()
        });
    }

    let encoded = match target_line_ending {
        LineEnding::Lf => encoded,
        LineEnding::CrLf => crate::encoding::denormalize_lf_to_crlf(&encoded, file_encoding),
        LineEnding::Cr => crate::encoding::denormalize_lf_to_cr(&encoded, file_encoding),
    };
    let out_bytes = if git_policy.write_bom(had_bom) {
        let bom = crate::encoding::bom_bytes_for(file_encoding);
        let mut bytes = Vec::with_capacity(bom.len() + encoded.len());
        bytes.extend_from_slice(bom);
        bytes.extend_from_slice(&encoded);
        bytes
    } else {
        encoded
    };

    // Computed for every preview, not just the real run: "would this write
    // change the file" is the question a preview exists to answer, and an
    // LF-space diff cannot answer it for a pure line-ending rewrite (both
    // sides are identical there).
    let would_write = crate::retry_io(|| std::fs::read(file))? != out_bytes;

    // --count: return match counts without applying any edits.
    if count_only {
        return Ok(ReplaceOutcome {
            counts,
            before,
            after,
            would_write,
            ..Default::default()
        });
    }

    // Write atomically: temp file in same dir → rename original to .bak →
    // persist temp to original path.  Skipped for --dry-run.
    let mut wrote = false;
    if !dry_run && would_write {
        crate::atomic_write(file, &out_bytes)?;
        wrote = true;
    }

    let (changed_lines, changed_lines_truncated) = if want_changed_lines {
        collect_changed_lines(&old_text, new_text, changed_lines_max)
    } else {
        (Vec::new(), false)
    };

    // Emit the diff (for both --diff after a successful write and --dry-run).
    if let Some(out) = diff_out {
        emit_unified_diff(file, old_norm, &new_norm, out)?;
    }

    Ok(ReplaceOutcome {
        counts,
        before,
        after,
        changed_lines,
        changed_lines_truncated,
        wrote,
        would_write,
    })
}

/// Pair up deleted and inserted lines from a line-level diff.
///
/// `similar` reports a replacement as a Delete run followed by an Insert run,
/// so the two runs are zipped positionally: the common case (one line edited
/// in place) then reads as a single entry with both sides populated, and a
/// genuine insertion or deletion keeps the unmatched side `None`.
///
/// Buffering is bounded by what `max` could still emit. A whole-file rewrite
/// is one enormous Delete run followed by one enormous Insert run with no
/// `Equal` between them, so an unbounded buffer would hold a copy of both
/// files just to discard all but the first `max` entries at the flush.
fn collect_changed_lines(
    old_text: &str,
    new_text: &str,
    max: Option<usize>,
) -> (Vec<ChangedLine>, bool) {
    use similar::ChangeTag;

    let diff = similar::TextDiff::from_lines(old_text, new_text);
    let mut out: Vec<ChangedLine> = Vec::new();
    let mut deletes: Vec<(usize, String)> = Vec::new();
    let mut inserts: Vec<(usize, String)> = Vec::new();
    let mut truncated = false;

    // `flush` is called at every boundary between changed and unchanged
    // content, which is what makes a Delete-then-Insert run one hunk.
    let flush = |deletes: &mut Vec<(usize, String)>,
                 inserts: &mut Vec<(usize, String)>,
                 out: &mut Vec<ChangedLine>,
                 truncated: &mut bool| {
        for index in 0..deletes.len().max(inserts.len()) {
            if max.is_some_and(|m| out.len() >= m) {
                *truncated = true;
                break;
            }
            let old = deletes.get(index);
            let new = inserts.get(index);
            out.push(ChangedLine {
                old_line: old.map(|(n, _)| n + 1),
                new_line: new.map(|(n, _)| n + 1),
                old_text: old.map(|(_, t)| t.clone()),
                new_text: new.map(|(_, t)| t.clone()),
            });
        }
        deletes.clear();
        inserts.clear();
    };

    for change in diff.iter_all_changes() {
        if matches!(change.tag(), ChangeTag::Equal) {
            flush(&mut deletes, &mut inserts, &mut out, &mut truncated);
            continue;
        }
        // `out.len()` is stable across a run (nothing is emitted until the
        // flush), so this is the most either side could still contribute.
        let budget = max.map(|m| m.saturating_sub(out.len()));
        let side = match change.tag() {
            ChangeTag::Delete => &mut deletes,
            _ => &mut inserts,
        };
        if budget.is_some_and(|b| side.len() >= b) {
            truncated = true;
            continue;
        }
        let value = change.value().trim_end_matches(['\n', '\r']).to_owned();
        let index = match change.tag() {
            ChangeTag::Delete => change.old_index(),
            _ => change.new_index(),
        };
        side.push((index.unwrap_or(0), value));
    }
    flush(&mut deletes, &mut inserts, &mut out, &mut truncated);
    (out, truncated)
}

/// Count matches of `re` in `haystack`, populating `regions` as it goes.
///
/// Matches are visited in left-to-right order, so `line_no`/`scanned_to` track
/// cumulative line position incrementally: each match only counts newlines in
/// the *unscanned* gap since the last match, never rescanning from the start
/// of the file. Total newline-counting work across all matches is therefore a
/// single linear pass over the file, the same order as reading it once for
/// matching.
fn collect_regions(
    re: &Regex,
    replacement: &[u8],
    has_capture_groups: bool,
    haystack: &[u8],
    regions: &mut Option<RegionsRequest<'_>>,
) -> usize {
    let mut line_no: usize = 1;
    let mut scanned_to: usize = 0;
    let mut text_materialized: usize = 0;
    let mut replacement_count = 0;
    for caps in re.captures_iter(haystack) {
        let m = caps.get(0).unwrap();

        if let Some(req) = regions.as_mut() {
            // Expand capture-group back-references in normalised space, but
            // only when a `ChangedRegion` echo was actually requested: the
            // real rewrite below (`re.replace_all`) redoes this expansion
            // independently, so doing it here unconditionally for every
            // match would be wasted work whenever `regions` is `None`
            // (e.g. `count`/`dry_run` without a preview).  When the pattern
            // has no explicit groups the replacement is taken verbatim, so
            // `$` survives instead of being read as a reference.
            let mut norm_repl: Vec<u8> = Vec::new();
            if has_capture_groups {
                caps.expand(replacement, &mut norm_repl);
            } else {
                norm_repl.extend_from_slice(replacement);
            }

            line_no += haystack[scanned_to..m.start()]
                .iter()
                .filter(|&&b| b == b'\n')
                .count();
            let start_line = line_no;
            // A newline that is the LAST byte of the match only terminates
            // the match's own last line -- it doesn't pull in any content
            // from the following line, so it must not extend end_line.
            let match_span = &haystack[m.start()..m.end()];
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
            // Bound retained text to `text_budget_lines` lines total (see
            // RegionsRequest doc) rather than the full size of every match's
            // replacement -- line-span numbers stay accurate either way,
            // only `new_text` is left empty once at/over budget. Checking
            // `text_materialized + new_line_count` (not just
            // `text_materialized`) against the budget matters: without it,
            // a single match whose own `new_line_count` dwarfs the budget
            // would still pass a `text_materialized < budget` check on the
            // first match and fully materialise a huge `new_text` even
            // though the echo can never use it.
            let under_budget = match req.text_budget_lines {
                None => true,
                Some(budget) => text_materialized.saturating_add(new_line_count) <= budget,
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

        replacement_count += 1;
    }
    replacement_count
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

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::{fs, io::Write as IoWrite, path::PathBuf};

    use tempfile::NamedTempFile;

    use super::*;

    // ── run_batch ───────────────────────────────────────────────────────────

    fn batch_op<'a>(pattern: &'a str, replacement: &'a [u8]) -> ReplaceOp<'a> {
        ReplaceOp {
            label: None,
            pattern,
            replacement,
            regex: false,
            multiline: false,
            allow_no_match: false,
        }
    }

    fn scratch(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn run_batch_applies_ops_in_order_against_the_evolving_buffer() {
        // The second op only matches text the first op produced, so a batch
        // that ran every op against the original buffer would report 0 for it.
        let f = scratch("alpha\n");
        let counts = run_batch(
            f.path(),
            &[batch_op("alpha", b"beta"), batch_op("beta", b"gamma")],
            None,
            ReplaceOptions::default(),
        )
        .unwrap()
        .counts;
        assert_eq!(counts, vec![1, 1]);
        assert_eq!(fs::read_to_string(f.path()).unwrap(), "gamma\n");
    }

    #[test]
    fn run_batch_refuses_everything_when_one_op_matches_nothing() {
        let f = scratch("alpha\nbeta\n");
        let err = run_batch(
            f.path(),
            &[
                batch_op("alpha", b"ALPHA"),
                batch_op("nowhere", b"X"),
                batch_op("beta", b"BETA"),
            ],
            None,
            ReplaceOptions::default(),
        )
        .expect_err("a zero-match op must refuse the batch");
        assert!(err.to_string().contains("matched 0 times"), "{err}");
        assert_eq!(
            fs::read_to_string(f.path()).unwrap(),
            "alpha\nbeta\n",
            "the file must be left untouched, not half-transformed"
        );
        let bak = PathBuf::from(format!("{}.bak", f.path().display()));
        assert!(!bak.exists(), "a refused batch must not leave a .bak");
    }

    /// `regex`/`multiline` are per-op in a batch. Silently dropping them gave
    /// a caller literal, non-multiline matching after asking for the opposite.
    #[test]
    fn run_batch_refuses_options_that_are_per_op() {
        let f = scratch("alpha\n");
        for opts in [
            ReplaceOptions {
                regex: true,
                ..Default::default()
            },
            ReplaceOptions {
                multiline: true,
                ..Default::default()
            },
        ] {
            let err = run_batch(f.path(), &[batch_op("alpha", b"beta")], None, opts)
                .expect_err("a per-op option set on the batch must be refused");
            assert!(err.to_string().contains("per-op in batch mode"), "{err}");
        }
        assert_eq!(fs::read_to_string(f.path()).unwrap(), "alpha\n");
    }

    #[test]
    fn run_batch_allow_no_match_exempts_only_its_own_op() {
        let f = scratch("alpha\n");
        let mut tolerant = batch_op("nowhere", b"X");
        tolerant.allow_no_match = true;
        let counts = run_batch(
            f.path(),
            &[batch_op("alpha", b"beta"), tolerant],
            None,
            ReplaceOptions::default(),
        )
        .unwrap()
        .counts;
        assert_eq!(counts, vec![1, 0]);
        assert_eq!(fs::read_to_string(f.path()).unwrap(), "beta\n");
    }

    #[test]
    fn run_batch_count_only_leaves_the_file_alone() {
        let f = scratch("alpha alpha beta\n");
        let counts = run_batch(
            f.path(),
            &[batch_op("alpha", b"x"), batch_op("beta", b"y")],
            None,
            ReplaceOptions {
                count_only: true,
                ..Default::default()
            },
        )
        .unwrap()
        .counts;
        assert_eq!(counts, vec![2, 1]);
        assert_eq!(fs::read_to_string(f.path()).unwrap(), "alpha alpha beta\n");
    }

    #[test]
    fn run_batch_rejects_an_empty_op_list() {
        let f = scratch("alpha\n");
        let err = run_batch(f.path(), &[], None, ReplaceOptions::default())
            .expect_err("an empty batch is a caller bug, not a no-op");
        assert!(err.to_string().contains("no operations supplied"), "{err}");
    }

    // ── line-ending census ──────────────────────────────────────────────────

    fn scratch_bytes(bytes: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    /// A replace rewrites the whole file, so a mixed file is silently
    /// collapsed onto one convention. The outcome has to say so — this is the
    /// case that otherwise only surfaces at `git commit`.
    #[test]
    fn outcome_reports_that_a_mixed_file_was_homogenised() {
        let f = scratch_bytes(b"alpha\nbeta\r\ngamma\n");
        let outcome = run_batch(
            f.path(),
            &[batch_op("alpha", b"ALPHA")],
            None,
            ReplaceOptions::default(),
        )
        .unwrap();

        assert_eq!((outcome.before.lf, outcome.before.crlf), (2, 1));
        assert!(outcome.before.is_mixed());
        assert_eq!(outcome.before.uniformity(), "mixed");

        assert!(!outcome.after.is_mixed());
        assert_eq!(outcome.after.uniformity(), "uniform");
        assert_eq!(outcome.after.terminators(), 3);
        assert!(
            outcome.normalized_line_endings(),
            "a mixed -> uniform rewrite must be flagged"
        );
    }

    #[test]
    fn outcome_does_not_claim_normalisation_for_an_already_uniform_file() {
        let f = scratch_bytes(b"alpha\nbeta\n");
        let outcome = run_batch(
            f.path(),
            &[batch_op("alpha", b"ALPHA")],
            None,
            ReplaceOptions::default(),
        )
        .unwrap();
        assert!(!outcome.normalized_line_endings());
        assert_eq!(outcome.before.uniformity(), "uniform");
        assert_eq!(outcome.after.uniformity(), "uniform");
        assert_eq!(
            (outcome.after.lf, outcome.after.crlf, outcome.after.cr),
            (2, 0, 0)
        );
    }

    /// `count: true` writes nothing but must still describe what a write
    /// would do, or a caller cannot use it to decide whether to write.
    #[test]
    fn count_only_reports_the_census_it_would_produce() {
        let f = scratch_bytes(b"alpha\nbeta\r\n");
        let outcome = run_batch(
            f.path(),
            &[batch_op("alpha", b"ALPHA")],
            None,
            ReplaceOptions {
                count_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(outcome.before.is_mixed());
        assert!(!outcome.after.is_mixed());
        assert_eq!(fs::read(f.path()).unwrap(), b"alpha\nbeta\r\n");
    }

    /// A run that matched nothing wrote nothing, so `after` must equal
    /// `before` rather than describing a hypothetical rewrite.
    #[test]
    fn a_no_op_run_leaves_the_census_unchanged() {
        let f = scratch_bytes(b"alpha\nbeta\r\n");
        let mut tolerant = batch_op("nowhere", b"X");
        tolerant.allow_no_match = true;
        let outcome = run_batch(f.path(), &[tolerant], None, ReplaceOptions::default()).unwrap();
        assert_eq!(outcome.before, outcome.after);
        assert!(!outcome.normalized_line_endings());
    }

    // ── changed-line images ─────────────────────────────────────────────────

    #[test]
    fn changed_lines_carry_both_images_and_both_positions() {
        let f = scratch("one\ntwo\nthree\n");
        let outcome = run_batch(
            f.path(),
            &[batch_op("two", b"TWO")],
            None,
            ReplaceOptions {
                changed_lines: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(outcome.changed_lines.len(), 1);
        let line = &outcome.changed_lines[0];
        assert_eq!(line.old_line, Some(2));
        assert_eq!(line.new_line, Some(2));
        assert_eq!(line.old_text.as_deref(), Some("two"));
        assert_eq!(line.new_text.as_deref(), Some("TWO"));
        assert!(!outcome.changed_lines_truncated);
    }

    /// Positions must describe the FINAL file, not an intermediate buffer, so
    /// a batch whose earlier op changed the line count still reports usable
    /// `new_line` values.
    #[test]
    fn changed_lines_positions_survive_a_line_count_change_mid_batch() {
        let f = scratch("one\ntwo\nthree\n");
        let outcome = run_batch(
            f.path(),
            &[
                batch_op("one\n", b"one\nEXTRA\n"),
                batch_op("three", b"THREE"),
            ],
            None,
            ReplaceOptions {
                changed_lines: true,
                ..Default::default()
            },
        )
        .unwrap();

        let three = outcome
            .changed_lines
            .iter()
            .find(|l| l.new_text.as_deref() == Some("THREE"))
            .expect("the renamed line must be reported");
        assert_eq!(three.old_line, Some(3), "position in the file as read");
        assert_eq!(three.new_line, Some(4), "position in the file as written");
    }

    #[test]
    fn changed_lines_respects_its_cap_and_says_when_it_truncated() {
        let f = scratch("a\na\na\na\na\n");
        let outcome = run_batch(
            f.path(),
            &[batch_op("a", b"b")],
            None,
            ReplaceOptions {
                changed_lines: true,
                changed_lines_max: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(outcome.changed_lines.len(), 2);
        assert!(outcome.changed_lines_truncated);
    }

    #[test]
    fn changed_lines_are_not_collected_unless_asked_for() {
        let f = scratch("one\ntwo\n");
        let outcome = run_batch(
            f.path(),
            &[batch_op("two", b"TWO")],
            None,
            ReplaceOptions::default(),
        )
        .unwrap();
        assert!(outcome.changed_lines.is_empty());
    }

    /// A whole-file rewrite is one Delete run then one Insert run with no
    /// `Equal` between them, so an unbounded buffer would hold both copies of
    /// the file just to discard all but `max` entries at the flush.
    #[test]
    fn changed_lines_does_not_buffer_a_whole_rewrite_to_emit_a_few() {
        let old: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..500).map(|i| format!("LINE {i}\n")).collect();
        let (lines, truncated) = collect_changed_lines(&old, &new, Some(3));
        assert_eq!(lines.len(), 3);
        assert!(truncated);
        assert_eq!(lines[0].old_text.as_deref(), Some("line 0"));
        assert_eq!(lines[0].new_text.as_deref(), Some("LINE 0"));
    }

    #[test]
    fn changed_lines_reports_a_pure_insertion_with_no_old_side() {
        let (lines, _) = collect_changed_lines("a\nb\n", "a\nNEW\nb\n", None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].old_line, None);
        assert_eq!(lines[0].old_text, None);
        assert_eq!(lines[0].new_line, Some(2));
        assert_eq!(lines[0].new_text.as_deref(), Some("NEW"));
    }

    #[test]
    fn changed_lines_reports_a_pure_deletion_with_no_new_side() {
        let (lines, _) = collect_changed_lines("a\ngone\nb\n", "a\nb\n", None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].old_line, Some(2));
        assert_eq!(lines[0].old_text.as_deref(), Some("gone"));
        assert_eq!(lines[0].new_line, None);
    }

    // ── preview modes never refuse, and never over-promise ──────────────────

    /// `count`/`dry_run` are introspection modes: a zero result is a
    /// legitimate answer, so they must not inherit the batch's all-or-nothing
    /// refusal. The single-op path has always behaved this way.
    #[test]
    fn preview_modes_do_not_refuse_a_zero_match_op() {
        let f = scratch("alpha\n");
        for opts in [
            ReplaceOptions {
                count_only: true,
                ..Default::default()
            },
            ReplaceOptions {
                dry_run: true,
                ..Default::default()
            },
        ] {
            let outcome = run_batch(
                f.path(),
                &[batch_op("alpha", b"ALPHA"), batch_op("nowhere", b"X")],
                None,
                opts,
            )
            .expect("a preview must not refuse");
            assert_eq!(outcome.counts, vec![1, 0]);
        }
        assert_eq!(fs::read_to_string(f.path()).unwrap(), "alpha\n");
    }

    /// A preview that matched nothing must not claim the write would
    /// normalise the file: a real run short-circuits and never rewrites.
    #[test]
    fn count_only_with_no_matches_does_not_promise_a_rewrite() {
        let f = scratch_bytes(b"a\nb\r\n");
        let mut tolerant = batch_op("nowhere", b"X");
        tolerant.allow_no_match = true;
        let outcome = run_batch(
            f.path(),
            &[tolerant],
            None,
            ReplaceOptions {
                count_only: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(outcome.before, outcome.after);
        assert!(!outcome.normalized_line_endings());
    }

    /// An identity substitution matches but produces byte-identical output,
    /// which the write path skips; callers that stamp mtime on "something
    /// happened" must be able to tell.
    #[test]
    fn an_identity_substitution_reports_that_nothing_was_written() {
        let f = scratch("alpha\n");
        let outcome = run_batch(
            f.path(),
            &[batch_op("alpha", b"alpha")],
            None,
            ReplaceOptions::default(),
        )
        .unwrap();
        assert_eq!(outcome.total(), 1, "it matched");
        assert!(!outcome.wrote, "but no bytes reached the disk");
    }

    #[test]
    fn a_real_substitution_reports_that_it_wrote() {
        let f = scratch("alpha\n");
        let outcome = run_batch(
            f.path(),
            &[batch_op("alpha", b"beta")],
            None,
            ReplaceOptions::default(),
        )
        .unwrap();
        assert!(outcome.wrote);
    }

    // ── previews must predict the write, including its failures ─────────────

    /// The write refuses content that introduces mojibake. A preview that
    /// skipped that guard would hand back an all-clear for exactly the
    /// corruption class this crate exists to prevent.
    #[test]
    fn previews_run_the_mojibake_guard_the_write_would_run() {
        for opts in [
            ReplaceOptions {
                count_only: true,
                ..Default::default()
            },
            ReplaceOptions {
                dry_run: true,
                ..Default::default()
            },
            ReplaceOptions::default(),
        ] {
            let f = scratch("cafe\n");
            let err = run_batch(
                f.path(),
                // Latin-1 mojibake digraph: valid UTF-8, and a fingerprint the
                // write-time guard rejects.
                &[batch_op("cafe", "caf\u{c3}\u{a9}".as_bytes())],
                None,
                opts,
            );
            assert!(err.is_err(), "every mode must refuse introduced mojibake");
            assert_eq!(fs::read_to_string(f.path()).unwrap(), "cafe\n");
        }
    }

    /// A pure line-ending rewrite is invisible to an LF-space diff, so
    /// `would_write` is the only honest answer to "will this change the file".
    #[test]
    fn a_line_ending_override_alone_reports_that_it_would_write() {
        let f = scratch_bytes(b"a\nb\n");
        let mut tolerant = batch_op("nowhere", b"X");
        tolerant.allow_no_match = true;
        let outcome = run_batch(
            f.path(),
            &[tolerant],
            None,
            ReplaceOptions {
                dry_run: true,
                line_ending_override: Some(LineEnding::CrLf),
                ..Default::default()
            },
        )
        .expect("an override exempts the zero-match refusal");
        assert_eq!(outcome.total(), 0);
        assert!(
            outcome.would_write,
            "converting every terminator is a change"
        );
        assert!(!outcome.wrote, "dry_run writes nothing");
        assert_eq!(fs::read(f.path()).unwrap(), b"a\nb\n");
    }

    /// The single-op path exempts a line-ending override from the zero-match
    /// refusal. A batch must NOT: its promise is that a typo cannot produce a
    /// partial rewrite, and converting terminators says nothing about whether
    /// the patterns were right.
    #[test]
    fn a_line_ending_override_does_not_exempt_a_batch_from_the_zero_match_refusal() {
        let f = scratch_bytes(b"a\nb\n");
        let err = run_batch(
            f.path(),
            &[batch_op("nowhere", b"X")],
            None,
            ReplaceOptions {
                line_ending_override: Some(LineEnding::CrLf),
                ..Default::default()
            },
        )
        .expect_err("a typo must still refuse the batch");
        assert!(err.to_string().contains("matched 0 times"), "{err}");
        assert!(
            err.to_string().contains("allow_no_match"),
            "the message must name the escape hatch: {err}"
        );
        assert_eq!(
            fs::read(f.path()).unwrap(),
            b"a\nb\n",
            "the refusal must leave the file untouched, override or not"
        );
    }

    /// The escape hatch still works: an op that is legitimately idempotent
    /// says so, and the override then rewrites the terminators.
    #[test]
    fn an_override_with_allow_no_match_still_rewrites_the_terminators() {
        let f = scratch_bytes(b"a\nb\n");
        let mut tolerant = batch_op("nowhere", b"X");
        tolerant.allow_no_match = true;
        let outcome = run_batch(
            f.path(),
            &[tolerant],
            None,
            ReplaceOptions {
                line_ending_override: Some(LineEnding::CrLf),
                ..Default::default()
            },
        )
        .expect("an explicit allow_no_match is the documented way through");
        assert_eq!(outcome.total(), 0);
        assert!(outcome.wrote);
        assert_eq!(fs::read(f.path()).unwrap(), b"a\r\nb\r\n");
    }

    /// A CR reaching the substitution buffer survives into the denormaliser,
    /// whose precondition is LF-only input, and lands as `\r\r\n`.
    #[test]
    fn decode_replacement_strips_cr_so_a_crlf_target_cannot_double_it() {
        assert_eq!(decode_replacement("a\\r\\nb", false).unwrap(), b"a\nb");
        assert_eq!(decode_replacement("a\r\nb", true).unwrap(), b"a\nb");
        assert_eq!(decode_replacement("a\rb", true).unwrap(), b"a\nb");
    }

    #[test]
    fn ops_from_json_rejects_an_empty_literal_pattern() {
        let err = ops_from_json(&serde_json::json!([{ "pattern": "", "replacement": "x" }]))
            .expect_err("an empty literal pattern would rewrite the whole file");
        assert!(err.contains("every position"), "{err}");
    }

    #[test]
    fn ops_from_json_rejects_a_mistyped_label() {
        let err =
            ops_from_json(&serde_json::json!([{ "label": 3, "pattern": "a", "replacement": "b" }]))
                .expect_err("a mistyped label must not silently vanish from the tally");
        assert!(err.contains("'label'"), "{err}");
    }

    /// Under a `.gitattributes` regime the target convention is gitoxide's,
    /// not the file's own — and `count: true` must predict the same target the
    /// write uses, or a preview is worthless for deciding whether to write.
    #[test]
    fn the_reported_target_is_the_gitattributes_one_in_both_preview_and_write() {
        let repo = tempfile::TempDir::new().unwrap();
        gix::init(repo.path()).expect("git init");
        fs::write(repo.path().join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();
        let cfg_path = repo.path().join(".git").join("config");
        let mut cfg = fs::read_to_string(&cfg_path).unwrap_or_default();
        cfg.push_str("\n[core]\n\tautocrlf = false\n");
        fs::write(&cfg_path, cfg).unwrap();

        // An LF file in a repo whose attributes demand CRLF.
        let target = repo.path().join("note.txt");
        fs::write(&target, b"alpha\nbeta\n").unwrap();

        let preview = run_batch(
            &target,
            &[batch_op("alpha", b"ALPHA")],
            None,
            ReplaceOptions {
                count_only: true,
                io_mode: IoMode::Buffered,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            preview.after.dominant(),
            Some(LineEnding::CrLf),
            "the preview must report git's target, not the file's own LF"
        );

        let written = run_batch(
            &target,
            &[batch_op("alpha", b"ALPHA")],
            None,
            ReplaceOptions {
                io_mode: IoMode::Buffered,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            written.after, preview.after,
            "preview must predict the write"
        );
        assert_eq!(fs::read(&target).unwrap(), b"ALPHA\r\nbeta\r\n");
    }

    // ── the shared `ops` schema ─────────────────────────────────────────────

    #[test]
    fn ops_from_json_accepts_the_mcp_ops_array_verbatim() {
        let ops = ops_from_json(&serde_json::json!([
            { "label": "rename", "pattern": "a(", "replacement": "b(" },
            { "pattern": "^x", "replacement": "y", "regex": true, "multiline": true },
        ]))
        .unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].label.as_deref(), Some("rename"));
        assert_eq!(ops[0].pattern, "a(");
        assert!(!ops[0].regex, "regex must stay opt-in per op");
        assert!(ops[1].regex && ops[1].multiline);
        assert_eq!(ops[1].label, None);
    }

    #[test]
    fn ops_from_json_takes_the_replacement_verbatim_by_default() {
        let ops = ops_from_json(&serde_json::json!([
            { "pattern": "x", "replacement": "a\\nb" },
            { "pattern": "y", "replacement": "a\\nb", "expand_escapes": true },
        ]))
        .unwrap();
        assert_eq!(ops[0].replacement, "a\\nb");
        assert_eq!(ops[1].replacement, "a\nb");
    }

    #[test]
    fn ops_from_json_decodes_a_base64_payload() {
        let ops = ops_from_json(&serde_json::json!([{
            "pattern": "eA==",
            "pattern_format": "base64",
            "replacement": "eQ==",
            "replacement_format": "base64",
        }]))
        .unwrap();
        assert_eq!(ops[0].pattern, "x");
        assert_eq!(ops[0].replacement, "y");
    }

    #[test]
    fn ops_from_json_rejects_an_unknown_field() {
        // A typo'd or MCP-only field must not be silently ignored, or a batch
        // that looks configured one way would run another.
        let err = ops_from_json(&serde_json::json!([
            { "pattern": "x", "replacement": "y", "regexp": true },
        ]))
        .expect_err("unknown fields must be rejected");
        assert!(err.contains("ops[0]"), "{err}");
        assert!(err.contains("regexp"), "{err}");
    }

    #[test]
    fn ops_from_json_rejects_a_stringly_typed_boolean() {
        let err = ops_from_json(&serde_json::json!([
            { "pattern": "x", "replacement": "y", "regex": "true" },
        ]))
        .expect_err("a JSON string is not a boolean");
        assert!(err.contains("'regex'"), "{err}");
    }

    #[test]
    fn ops_from_json_rejects_expand_escapes_with_a_format() {
        let err = ops_from_json(&serde_json::json!([{
            "pattern": "x",
            "replacement": "eQ==",
            "replacement_format": "base64",
            "expand_escapes": true,
        }]))
        .expect_err("the two escape channels are contradictory");
        assert!(err.contains("expand_escapes"), "{err}");
    }

    #[test]
    fn ops_from_json_rejects_an_empty_array() {
        assert!(ops_from_json(&serde_json::json!([])).is_err());
    }

    #[test]
    fn run_batch_validates_every_op_before_touching_the_file() {
        // The bad pattern is last, so a batch that compiled lazily would have
        // already rewritten the file by the time it failed.
        let f = scratch("alpha\n");
        let mut bad = batch_op("(unclosed", b"x");
        bad.regex = true;
        let err = run_batch(
            f.path(),
            &[batch_op("alpha", b"beta"), bad],
            None,
            ReplaceOptions::default(),
        )
        .expect_err("an invalid regex must fail the batch");
        let _ = err;
        assert_eq!(fs::read_to_string(f.path()).unwrap(), "alpha\n");
    }

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
        Ok(run(
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
                changed_lines: false,
                changed_lines_max: None,
            },
        )?
        .total())
    }

    /// `run` takes `replacement` as `&[u8]` for signature convenience, but
    /// the bytes must form valid UTF-8: `replace` operates on decoded UTF-8
    /// text throughout and re-encodes strictly afterward, so a replacement
    /// that isn't valid UTF-8 on its own has nothing valid to expand into.
    /// A single `\xFF` byte (as `--literal-replacement` or an unpaired
    /// `\xHH` escape would produce) is invalid UTF-8 in isolation, so `run`
    /// must reject it explicitly rather than let it corrupt the match
    /// expansion or panic downstream. The file must be left untouched.
    #[test]
    fn run_rejects_non_utf8_replacement_bytes() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"hello\n").unwrap();
        f.flush().unwrap();
        let path = f.path().to_path_buf();
        drop(f);
        fs::write(&path, b"hello\n").unwrap();

        let err = run_test(
            &path, "hello", b"\xFF", false, false, None, None, false, false,
        )
        .expect_err("a lone 0xFF byte is not valid UTF-8 and must be rejected");
        assert!(
            err.to_string().contains("replacement is not valid UTF-8"),
            "got: {err}"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            b"hello\n",
            "file must be untouched when the replacement is rejected"
        );

        let _ = fs::remove_file(format!("{}.bak", path.display()));
        let _ = fs::remove_file(&path);
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

    /// Regression: a single match whose own replacement dwarfs the budget
    /// must not be fully materialised just because the running total was
    /// still under budget beforehand -- the check must account for the
    /// candidate region's own size (`text_materialized + new_line_count`),
    /// not just the pre-match running total, or a huge first match defeats
    /// the memory bound entirely.
    #[test]
    fn changed_region_single_huge_match_is_not_materialized_over_budget() {
        let content = b"foo\nkeep\n";
        let huge_replacement = "x\n".repeat(1000);
        let regions =
            replace_file_regions(content, "foo", huge_replacement.as_bytes(), false, Some(5));
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].new_line_count, 1000);
        assert_eq!(
            regions[0].new_text, "",
            "a single match whose new_line_count alone exceeds the budget must \
             not be materialized, even though the running total started at 0"
        );
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

    #[test]
    fn zero_match_preserves_mtime_and_writes_no_bak() {
        // Setup: write a file, capture its mtime, ensure any stray .bak
        // from a prior test in the same tmpdir is gone.
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"hello world\n").unwrap();
        f.flush().unwrap();
        let path = f.path().to_path_buf();
        drop(f);
        fs::write(&path, b"hello world\n").unwrap();
        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&bak);

        let before_mtime = fs::metadata(&path).unwrap().modified().unwrap();

        // Sleep briefly so that if the code path DID rewrite the file, its
        // mtime would differ from the captured one -- otherwise a fast test
        // machine could produce a false pass on same-timestamp reads.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let n = run_test(
            &path,
            "this_pattern_is_not_in_the_file",
            b"REPLACEMENT",
            false,
            false,
            None,
            None,
            false,
            false,
        )
        .unwrap();
        assert_eq!(n, 0, "zero-match run must return 0");

        let after_mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            before_mtime, after_mtime,
            "zero-match run must not bump the file's mtime"
        );
        assert!(
            !std::path::Path::new(&bak).exists(),
            "zero-match run must not create <file>.bak"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            b"hello world\n",
            "zero-match run must leave file bytes untouched"
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn zero_match_does_not_apply_automatic_git_eol_policy() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        fs::write(dir.path().join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();
        let path = dir.path().join("note.txt");
        fs::write(&path, b"hello\n").unwrap();

        let count = run_test(
            &path,
            "not present",
            b"replacement",
            false,
            false,
            None,
            None,
            false,
            false,
        )
        .unwrap();

        assert_eq!(count, 0);
        assert_eq!(fs::read(&path).unwrap(), b"hello\n");
        assert!(!Path::new(&format!("{}.bak", path.display())).exists());
    }

    #[test]
    fn text_auto_does_not_normalize_binary_content() {
        let dir = tempfile::tempdir().unwrap();
        gix::init(dir.path()).unwrap();
        fs::write(
            dir.path().join(".gitattributes"),
            "*.dat text=auto eol=crlf\n",
        )
        .unwrap();
        let path = dir.path().join("binary.dat");
        fs::write(&path, b"old\n\0tail\n").unwrap();

        run_test(&path, "old", b"new", false, false, None, None, false, false).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new\n\0tail\n");
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
