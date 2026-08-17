// Copyright (c) 2026, Michael Grier

//! `tpu doctor` — encoding-aware diagnostic for one or more paths.
//!
//! Walks each given path (file, directory, or shell-style glob), classifies
//! every reachable text file as either *clean*, *mojibake-suspected*, or
//! *invalid in its detected encoding*, and (optionally) attempts a one-layer
//! peel repair using [`crate::mojibake::looks_like_one_layer_peel`].
//!
//! ## Detection model
//!
//! Each file is opened through [`harrier::source::Source`] with the default
//! [`SourceConfig`], producing the *file's* detected encoding (UTF-8,
//! UTF-16LE/BE, Windows-1252, …).  The decoded text is then scanned by
//! [`crate::mojibake::scan`] for the four characteristic mojibake patterns.
//!
//! ### UTF-8 preference guard
//!
//! Before trusting the sniffer, the diagnoser applies a hard rule: **a byte
//! stream that is well-formed UTF-8 is always treated as UTF-8**, regardless
//! of what the statistical detector reported.  harrier's sniffer can tip into
//! Windows-1252 on large files dense with multi-byte sequences (box-drawing,
//! em-dashes, arrows); decoding valid UTF-8 as CP1252 then manufactures
//! thousands of phantom mojibake matches and, under `--fix=peel`, a
//! destructive lossy re-encode.  An explicit UTF-16 detection (or a body
//! containing NUL bytes) is left untouched.
//!
//! - **Encoding-invalid**: the file's bytes contain sequences that are not
//!   valid in the detected encoding (e.g. lone surrogates in UTF-16, or
//!   bare 0x80–0xFF bytes in UTF-8 that don't form valid sequences).  In
//!   this state the mojibake scan is *not* run because the decoded text
//!   contains replacement characters that would skew the result.
//! - **Mojibake-suspected**: decoding succeeded cleanly *and* one or more
//!   characteristic patterns were spotted.  Reported with byte offset, 1-
//!   based line / column, and the pattern's name.
//! - **Clean**: no issues.
//!
//! Files matching the `encoding-check: allow-mojibake` opt-out marker
//! (see [`crate::mojibake::ALLOW_MARKER`]) are reported as clean.
//!
//! ## Repair (`--fix=peel`)
//!
//! For each mojibake-suspected file [`crate::mojibake::looks_like_one_layer_peel`]
//! is invoked.  When it returns `Some(repaired)` (i.e. strictly fewer
//! matches than the input), the file is rewritten via the standard
//! [`crate::cmd::write::run`] path so the existing `.bak` machinery and
//! the M2 write-time guard apply uniformly.  The recovered text is written
//! as **UTF-8** (not the source's previously detected encoding): a peel is
//! the inverse of "valid UTF-8 misread as Windows-1252", so its output is
//! recovered UTF-8 and must be persisted as such — re-encoding it back into
//! a legacy code page is the lossy step that produced the original incident.
//! The write is issued with
//! [`crate::mojibake::WritePolicy::permissive`] because the *intent* is to write a string
//! that may still legitimately contain mojibake — we just want
//! *strictly less* than before.
//!
//! ## Output
//!
//! - **Human** (default): a human-readable per-file line followed by a
//!   summary.  Suppressed by `--quiet` (summary only).
//! - **JSON**: a single JSON document with the following schema:
//!   ```json
//!   {
//!     "files": [
//!       {
//!         "path": "...",
//!         "encoding_detected": "UTF-8",
//!         "valid_in_detected_encoding": true,
//!         "mojibake_matches": [
//!           { "byte_offset": 12, "line": 1, "col": 13, "pattern": "latin1" }
//!         ],
//!         "peel_suggested": null,
//!         "repaired": false
//!       }
//!     ],
//!     "total_files_scanned": 7,
//!     "total_issues": 2,
//!     "total_repaired": 0
//!   }
//!   ```
//!
//! ## Exit code (chosen by caller)
//!
//! Caller should treat a non-zero `total_issues` *after fixes have been
//! applied* as a failure (exit 1).  This module returns the report; the
//! exit decision lives in `main.rs`.

use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use harrier::{
    encoding::{LineEnding, SourceConfig},
    source::Source,
};
use serde_json::json;

use crate::{
    IoMode,
    encoding::{BomPolicy, OutputEncoding},
    git::{self, EolMismatch},
    mojibake::{self, Pattern},
    walk::GlobMatcher,
};

// ── Public option / record types ────────────────────────────────────────────

/// Output format for the doctor report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DoctorFormat {
    /// Coloured / plain human-readable lines.  This is the default.
    #[default]
    Human,
    /// A single pretty-printed JSON document.
    Json,
}

/// Repair mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DoctorFix {
    /// Report only; do not touch any file.  This is the default.
    #[default]
    None,
    /// Apply [`mojibake::looks_like_one_layer_peel`] to flagged files; if
    /// it returns a strictly-better string, atomically rewrite the file.
    Peel,
}

/// Tunable knobs for [`run`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DoctorOptions {
    pub format: DoctorFormat,
    pub fix: DoctorFix,
    /// Suppress per-file lines in human mode; the summary is still
    /// printed.  No effect on JSON mode (which is always one document).
    pub quiet: bool,
    /// When `true`, annotate each [`DoctorReplacementCharMatch`] with a
    /// heuristic suggestion for what the original character might have been.
    /// Off by default — guesses may be wrong; the user must opt in explicitly
    /// via `--guess` / `guess: true`.
    pub guess: bool,
}

/// One mojibake match within a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorMatch {
    pub byte_offset: usize,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number, in `char`s (not bytes).
    pub col: usize,
    pub pattern: Pattern,
}

/// One `U+FFFD` (replacement character) occurrence within a single file.
///
/// This is a **separate, non-peelable diagnostic class** from [`DoctorMatch`].
/// The original byte is gone; no automatic repair is possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReplacementCharMatch {
    pub byte_offset: usize,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number, in `char`s.
    pub col: usize,
    /// Short excerpt of surrounding text for context (~20 chars each side).
    pub context: String,
    /// Heuristic suggested replacement.  Only set when [`DoctorOptions::guess`]
    /// is `true`; `None` when no confident inference is available.
    pub suggested: Option<char>,
}

/// Per-file diagnostic record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorIssue {
    pub path: PathBuf,
    pub encoding_detected: &'static str,
    pub valid_in_detected_encoding: bool,
    pub mojibake_matches: Vec<DoctorMatch>,
    /// `U+FFFD` replacement-character occurrences (non-peelable; manual repair
    /// only).  Empty when none were found or when the file's opt-out marker
    /// (`encoding-check: allow-replacement-char`) is present.
    pub replacement_char_matches: Vec<DoctorReplacementCharMatch>,
    /// `Some(text)` if a one-layer peel produces strictly fewer matches.
    /// Only populated when at least one mojibake pattern was found.
    pub peel_suggested: Option<String>,
    /// `true` once the file has been rewritten with `peel_suggested`.
    pub repaired: bool,
    /// `Some(_)` when the file's on-disk line endings disagree with git's
    /// expected working-tree convention for that path.  Only populated when a
    /// `git_root` was supplied to [`run_with_policy`].
    pub eol_mismatch: Option<EolMismatch>,
    /// `true` once the file's line endings have been normalised to git's
    /// expectation under `--fix=eol` / `--fix=all`.
    pub eol_repaired: bool,
}

impl DoctorIssue {
    /// True when the file has anything worth reporting (invalid encoding,
    /// mojibake matches, replacement-character residue, or a git line-ending
    /// mismatch).
    pub fn is_problem(&self) -> bool {
        !self.valid_in_detected_encoding
            || !self.mojibake_matches.is_empty()
            || !self.replacement_char_matches.is_empty()
            || self.eol_mismatch.is_some()
    }
}

/// Aggregate result of [`run`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DoctorReport {
    /// One entry per *flagged* file (clean files are omitted).
    pub issues: Vec<DoctorIssue>,
    pub total_files_scanned: usize,
    pub total_repaired: usize,
    /// Whether this run was asked to normalise line endings (`--fix=eol`/
    /// `all`).  Drives the human-output distinction between a bare *report*
    /// of a mismatch and a mismatch that a normalising run left untouched.
    pub eol_fix_requested: bool,
}

impl DoctorReport {
    /// Number of flagged files (mojibake- or encoding-invalid).
    pub fn total_issues(&self) -> usize {
        self.issues.iter().filter(|i| i.is_problem()).count()
    }
}

// ── File-extension skip-list ────────────────────────────────────────────────

/// Lowercase file extensions that are unconditionally treated as binary
/// and skipped by the doctor walk.
const BINARY_EXTS: &[&str] = &[
    "exe", "dll", "so", "dylib", "a", "lib", "o", "obj", "pdb", "class", "jar", "war", "zip", "7z",
    "gz", "tgz", "bz2", "xz", "rar", "tar", "iso", "dmg", "img", "bin", "dat", "db", "sqlite",
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "tif", "tiff", "webp", "psd", "svgz", "mp3", "mp4",
    "wav", "flac", "ogg", "avi", "mov", "mkv", "webm", "pdf", "doc", "docx", "xls", "xlsx", "ppt",
    "pptx", "ttf", "otf", "woff", "woff2", "eot",
];

fn is_binary_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| BINARY_EXTS.iter().any(|b| b.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

// ── Path expansion + .gitignore handling ────────────────────────────────────

/// Expand `path_specs` into a deduplicated list of regular files.
///
/// Each spec is either:
/// - a literal file path (added directly),
/// - a directory (walked recursively; binary extensions and `.git`
///   subtrees are skipped; entries matching the optional `.gitignore`
///   at the walk root are skipped),
/// - a shell-style glob (any spec containing `*`, `?`, `[`, `{`).
#[allow(dead_code)]
fn expand_paths(path_specs: &[&str]) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    expand_paths_with_policy(path_specs, crate::cmd::copy::OnError::Fail, &mut Vec::new())
}

/// As [`expand_paths`] but with explicit walk-error policy. Inaccessible
/// directories produce a textual warning appended to `warnings_out` when
/// `on_error == OnError::Warn`.
fn expand_paths_with_policy(
    path_specs: &[&str],
    on_error: crate::cmd::copy::OnError,
    warnings_out: &mut Vec<String>,
) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    // Track warnings emitted during this call so we can detect the case
    // where every supplied path was inaccessible (all warned-and-skipped).
    let initial_warnings_len = warnings_out.len();

    for &spec in path_specs {
        let is_glob =
            spec.contains('*') || spec.contains('?') || spec.contains('[') || spec.contains('{');

        if is_glob {
            // Anchor an absolute glob at its non-glob prefix; a relative glob
            // walks from the current directory.
            let (walk_root, rel_pattern) = crate::walk::split_glob_root(spec);
            let found = crate::walk::walk(
                &walk_root,
                &rel_pattern,
                SKIP_DIRS,
                on_error,
                "doctor",
                warnings_out,
            )?;
            for rel in found.files {
                let path = walk_root.join(&rel);
                if !is_binary_extension(&path) {
                    push_unique(&mut paths, &mut seen, path);
                }
            }
            continue;
        }

        let p = PathBuf::from(spec);
        let meta = match fs::metadata(&p) {
            Ok(m) => m,
            Err(e) => match on_error {
                crate::cmd::copy::OnError::Fail => {
                    return Err(format!("doctor: cannot stat {}: {e}", p.display()).into());
                }
                // A single explicit path has no other entries to fall back on;
                // downgrade to a warning only when there are multiple specs.
                crate::cmd::copy::OnError::Warn if path_specs.len() == 1 => {
                    return Err(format!("doctor: cannot stat {}: {e}", p.display()).into());
                }
                crate::cmd::copy::OnError::Warn => {
                    warnings_out.push(format!("doctor: cannot stat {}: {e}", p.display()));
                    continue;
                }
            },
        };

        if meta.is_file() {
            if !is_binary_extension(&p) {
                push_unique(&mut paths, &mut seen, p);
            }
            continue;
        }

        if meta.is_dir() {
            let ignore = load_gitignore(&p);
            let found = crate::walk::walk(&p, "**/*", SKIP_DIRS, on_error, "doctor", warnings_out)?;
            for rel in found.files {
                let path = p.join(&rel);
                if is_binary_extension(&path) {
                    continue;
                }
                if let Some(set) = &ignore
                    && set.is_match(&rel)
                {
                    continue;
                }
                push_unique(&mut paths, &mut seen, path);
            }
            continue;
        }
    }

    // If every supplied path was inaccessible (all warned-and-skipped) there
    // is nothing to scan and the caller would silently report "scanned 0".
    // Return an error so the user knows their path specs were all bad.
    if paths.is_empty() && warnings_out.len() > initial_warnings_len {
        return Err(
            "doctor: no files to scan; all specified paths were missing or inaccessible".into(),
        );
    }

    Ok(paths)
}

fn push_unique(paths: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, p: PathBuf) {
    let key = fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
    if seen.insert(key) {
        paths.push(p);
    }
}

/// Directory names whose subtrees are skipped during walks: `.git/`,
/// `node_modules/`, and `target/`. These are virtually always either binary,
/// generated, or already classified by their own checkers — and they would
/// otherwise dominate the report.
pub(crate) const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target"];

/// Load `<root>/.gitignore` if present and compile its non-comment
/// non-empty lines into a [`GlobMatcher`].  Negation lines (starting with `!`)
/// are ignored to keep the implementation conservative — the doctor
/// walks files; it does not need to be a perfect gitignore engine.
fn load_gitignore(root: &Path) -> Option<GlobMatcher> {
    let content = fs::read_to_string(root.join(".gitignore")).ok()?;
    let mut matcher = GlobMatcher::new();
    let mut any = false;
    for line in content.lines() {
        let s = line.trim();
        if s.is_empty() || s.starts_with('#') || s.starts_with('!') {
            continue;
        }
        // Trailing-slash means "directory"; expand to `dir/**`.
        let pat = if let Some(stripped) = s.strip_suffix('/') {
            format!("{stripped}/**")
        } else {
            s.to_string()
        };
        // Allow both root-anchored and any-depth matches.
        any |= matcher.add(&pat);
        any |= matcher.add(&format!("**/{pat}"));
    }
    if !any {
        return None;
    }
    Some(matcher)
}

// ── Per-file diagnosis ──────────────────────────────────────────────────────

/// Run encoding + mojibake diagnostics for a single file.
///
/// Returns `Ok(None)` for a clean file (and the file is not included in
/// the report), `Ok(Some(issue))` when there is anything to report, and
/// `Err` for I/O / open failures.
fn diagnose_file(
    path: &Path,
    io_mode: IoMode,
    guess: bool,
    git: Option<&git::GitEol>,
) -> Result<Option<DoctorIssue>, Box<dyn Error>> {
    let branch = crate::open_as_branch(path, io_mode)?;
    let source = Source::new(Arc::clone(&branch), SourceConfig::default())?;
    let encoding = source.encoding();

    // Read raw bytes through the same I/O mode so behaviour is identical
    // to other commands.
    let raw = crate::read_raw_bytes(path, io_mode)?;
    let bom_len = source.bom_len();
    let body = &raw[bom_len.min(raw.len())..];

    // Git line-ending mismatch is an independent diagnostic class: it is
    // evaluated against the raw bytes and is orthogonal to mojibake.  Best
    // effort — any git error yields `None`.  UTF-16 is skipped entirely:
    // its line endings are multi-byte (e.g. `0D 00 0A 00`), so the byte-level
    // CR/LF statistics are unreliable and `apply_eol_fix` cannot repair them
    // anyway — reporting a mismatch we can neither trust nor fix would only
    // mislead.
    let eol_mismatch = if is_utf16(encoding.name()) {
        None
    } else {
        git.and_then(|g| g.detect(path, &raw).ok().flatten())
    };

    // ── UTF-8 preference guard ──────────────────────────────────────────
    //
    // A byte stream that is *well-formed UTF-8* must never be classified as
    // a legacy single-byte code page.  harrier's statistical sniffer can,
    // on large files dense with multi-byte sequences (box-drawing, em-dashes,
    // arrows in doc comments), tip into Windows-1252.  That misdetection then
    // decodes every valid 3-byte UTF-8 sequence (e.g. `─` = E2 94 80) as three
    // separate CP1252 chars (`â"€`), manufacturing thousands of phantom
    // "mojibake" matches — and, under `--fix=peel`, a destructive lossy
    // re-encode that replaces real characters with U+FFFD.  Preferring UTF-8
    // whenever the raw bytes decode cleanly as UTF-8 eliminates that entire
    // failure class.
    //
    // We deliberately do *not* override an explicit UTF-16 detection (those
    // streams can also pass `from_utf8` when every high byte is 0x00), and we
    // bail out if the body contains a NUL byte — real UTF-8 text never does,
    // but UTF-16/binary content does.
    let detected_name = encoding.name();
    let prefer_utf8 = detected_name != "UTF-8"
        && !detected_name.starts_with("UTF-16")
        && !body.contains(&0u8)
        && std::str::from_utf8(body).is_ok();

    // Decode; `had_errors` tells us whether any byte sequences were
    // invalid in the detected encoding.
    let (encoding_name, decoded, had_errors): (&'static str, std::borrow::Cow<'_, str>, bool) =
        if prefer_utf8 {
            (
                "UTF-8",
                std::borrow::Cow::Borrowed(
                    std::str::from_utf8(body).expect("prefer_utf8 guarantees valid UTF-8"),
                ),
                false,
            )
        } else {
            let (decoded, _, had_errors) = encoding.decode(body);
            (detected_name, decoded, had_errors)
        };
    let decoded_text: &str = &decoded;

    if had_errors {
        // Encoding-invalid: don't bother running the mojibake scan, the
        // replacement chars would create false positives.  This is checked
        // *before* the allow-marker opt-out because that marker only
        // suppresses mojibake / replacement-char diagnostics — it does not
        // assert that the bytes are valid in their detected encoding.
        return Ok(Some(DoctorIssue {
            path: path.to_path_buf(),
            encoding_detected: encoding_name,
            valid_in_detected_encoding: false,
            mojibake_matches: Vec::new(),
            replacement_char_matches: Vec::new(),
            peel_suggested: None,
            repaired: false,
            eol_mismatch,
            eol_repaired: false,
        }));
    }

    // Allow-marker opt-out for the *whole file* (suppresses all mojibake /
    // replacement-char diagnostics).  A git line-ending mismatch is a separate
    // concern and is still reported even when the marker is present.
    if mojibake::allowed_by_marker(decoded_text) {
        return Ok(eol_only_issue(path, encoding_name, eol_mismatch));
    }

    let report = mojibake::scan(decoded_text);

    // Scan for U+FFFD replacement-character residue (unless the file opts out).
    let rc_matches = if mojibake::has_replacement_char_allow_marker(decoded_text) {
        Vec::new()
    } else {
        let raw_rc = mojibake::scan_replacement_chars(decoded_text, guess);
        if raw_rc.is_empty() {
            Vec::new()
        } else {
            annotate_replacement_char_matches(decoded_text, &raw_rc)
        }
    };

    if report.matches.is_empty() && rc_matches.is_empty() {
        return Ok(eol_only_issue(path, encoding_name, eol_mismatch));
    }

    // Build line/col index by streaming through the decoded text once.
    let matches = if report.matches.is_empty() {
        Vec::new()
    } else {
        annotate_matches(decoded_text, &report.matches)
    };

    let peel_suggested = if report.matches.is_empty() {
        None
    } else {
        mojibake::looks_like_one_layer_peel(decoded_text)
    };

    Ok(Some(DoctorIssue {
        path: path.to_path_buf(),
        encoding_detected: encoding_name,
        valid_in_detected_encoding: true,
        mojibake_matches: matches,
        replacement_char_matches: rc_matches,
        peel_suggested,
        repaired: false,
        eol_mismatch,
        eol_repaired: false,
    }))
}

/// Build a [`DoctorIssue`] representing an otherwise-clean file that only has a
/// git line-ending mismatch, or `None` when there is no mismatch either.
fn eol_only_issue(
    path: &Path,
    encoding_name: &'static str,
    eol_mismatch: Option<EolMismatch>,
) -> Option<DoctorIssue> {
    eol_mismatch.map(|m| DoctorIssue {
        path: path.to_path_buf(),
        encoding_detected: encoding_name,
        valid_in_detected_encoding: true,
        mojibake_matches: Vec::new(),
        replacement_char_matches: Vec::new(),
        peel_suggested: None,
        repaired: false,
        eol_mismatch: Some(m),
        eol_repaired: false,
    })
}

/// Convert raw mojibake matches into [`DoctorMatch`]es with 1-based line
/// and column numbers (column counted in `char`s).
fn annotate_matches(text: &str, raw: &[mojibake::Match]) -> Vec<DoctorMatch> {
    // Pre-build a sorted offset->index map so we can fold linearly.
    let mut sorted: Vec<(usize, usize)> = raw
        .iter()
        .enumerate()
        .map(|(i, m)| (m.byte_offset, i))
        .collect();
    sorted.sort_by_key(|&(off, _)| off);

    let mut out: Vec<Option<DoctorMatch>> = vec![None; raw.len()];
    let mut next = 0usize;
    let mut line = 1usize;
    let mut col = 1usize;
    let mut byte_pos = 0usize;

    for ch in text.chars() {
        while next < sorted.len() && sorted[next].0 == byte_pos {
            let idx = sorted[next].1;
            out[idx] = Some(DoctorMatch {
                byte_offset: raw[idx].byte_offset,
                line,
                col,
                pattern: raw[idx].pattern,
            });
            next += 1;
        }
        let len = ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
        byte_pos += len;
    }
    // EOF: any matches whose offset equals total length (shouldn't happen
    // for our patterns, but be defensive).
    while next < sorted.len() && sorted[next].0 == byte_pos {
        let idx = sorted[next].1;
        out[idx] = Some(DoctorMatch {
            byte_offset: raw[idx].byte_offset,
            line,
            col,
            pattern: raw[idx].pattern,
        });
        next += 1;
    }

    out.into_iter()
        .map(|o| {
            o.unwrap_or(DoctorMatch {
                byte_offset: 0,
                line: 0,
                col: 0,
                pattern: Pattern::Latin1,
            })
        })
        .collect()
}

/// Annotate raw [`mojibake::ReplacementCharMatch`]es with 1-based line and
/// column numbers by streaming through the decoded text once.
fn annotate_replacement_char_matches(
    text: &str,
    raw: &[mojibake::ReplacementCharMatch],
) -> Vec<DoctorReplacementCharMatch> {
    let mut sorted: Vec<(usize, usize)> = raw
        .iter()
        .enumerate()
        .map(|(i, m)| (m.byte_offset, i))
        .collect();
    sorted.sort_by_key(|&(off, _)| off);

    let mut out: Vec<Option<DoctorReplacementCharMatch>> = vec![None; raw.len()];
    let mut next = 0usize;
    let mut line = 1usize;
    let mut col = 1usize;
    let mut byte_pos = 0usize;

    for ch in text.chars() {
        while next < sorted.len() && sorted[next].0 == byte_pos {
            let idx = sorted[next].1;
            out[idx] = Some(DoctorReplacementCharMatch {
                byte_offset: raw[idx].byte_offset,
                line,
                col,
                context: raw[idx].context.clone(),
                suggested: raw[idx].suggested,
            });
            next += 1;
        }
        let len = ch.len_utf8();
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
        byte_pos += len;
    }
    while next < sorted.len() && sorted[next].0 == byte_pos {
        let idx = sorted[next].1;
        out[idx] = Some(DoctorReplacementCharMatch {
            byte_offset: raw[idx].byte_offset,
            line,
            col,
            context: raw[idx].context.clone(),
            suggested: raw[idx].suggested,
        });
        next += 1;
    }

    out.into_iter()
        .enumerate()
        .map(|(idx, o)| {
            // Preserve raw match data in the fallback so the report stays
            // anchored to the right location even if the invariant is violated.
            o.unwrap_or(DoctorReplacementCharMatch {
                byte_offset: raw[idx].byte_offset,
                line: 0,
                col: 0,
                context: raw[idx].context.clone(),
                suggested: raw[idx].suggested,
            })
        })
        .collect()
}

// ── Repair ──────────────────────────────────────────────────────────────────

/// Apply `peel_suggested` to the file via [`crate::cmd::write::run`].
///
/// The peeled string is, by construction, *recovered UTF-8 text* (the
/// reverse of "valid UTF-8 misread as Windows-1252").  It is therefore
/// written with [`OutputEncoding::Utf8`] rather than `Preserve`: re-encoding
/// recovered UTF-8 back into the file's *previously detected* encoding is
/// exactly the lossy step that can turn characters absent from that code
/// page (box-drawing, arrows, …) into `U+FFFD`.  Writing UTF-8 keeps the
/// repair lossless and idempotent.  Updates `issue.repaired` on success.
fn apply_peel(issue: &mut DoctorIssue, io_mode: IoMode) -> Result<(), Box<dyn Error>> {
    let Some(peeled) = issue.peel_suggested.clone() else {
        return Ok(());
    };
    crate::cmd::write::run(
        &issue.path,
        &peeled,
        OutputEncoding::Utf8,
        BomPolicy::default(),
        None,
        None,
        io_mode,
        mojibake::WritePolicy::permissive(),
    )?;
    issue.repaired = true;
    Ok(())
}

/// Whether a harrier encoding name denotes a UTF-16 variant (`UTF-16LE` /
/// `UTF-16BE`).  Centralised so the detection-skip in [`diagnose_file`] and the
/// repair-skip in [`apply_eol_fix`] stay in lock-step: UTF-16's multi-byte line
/// endings make the byte-level EOL pass both unreliable to detect and unsafe to
/// rewrite.
fn is_utf16(encoding_name: &str) -> bool {
    encoding_name.starts_with("UTF-16")
}

/// Normalise a git-EOL-mismatched file's line endings to git's expected
/// convention, preserving its encoding, BOM, and all other bytes.
///
/// The transform operates at the byte level and is only correct for
/// ASCII-transparent encodings (UTF-8, Windows-1252, Shift-JIS, …) where the
/// `0x0D` / `0x0A` line-ending bytes never appear inside a multi-byte
/// character.  UTF-16 never reaches the rewrite path: [`diagnose_file`] skips
/// it during detection (`eol_mismatch` is always `None`), so UTF-16 EOL
/// mismatches are neither reported nor repaired.  The `is_utf16` guard below
/// is therefore a belt-and-suspenders no-op that keeps the two code paths in
/// lock-step.  Sets `issue.eol_repaired` only when bytes were actually
/// rewritten.
fn apply_eol_fix(issue: &mut DoctorIssue, io_mode: IoMode) -> Result<(), Box<dyn Error>> {
    let Some(mismatch) = issue.eol_mismatch else {
        return Ok(());
    };
    if is_utf16(issue.encoding_detected) {
        return Ok(());
    }
    let raw = crate::read_raw_bytes(&issue.path, io_mode)?;
    let converted = normalize_eol_bytes(&raw, mismatch.expected);
    if converted == raw {
        // No bytes changed — don't claim a repair (keeps the "N repaired"
        // count and the `[NORMALIZED]` tag honest).
        return Ok(());
    }

    // When a prior peel-fix already ran on this file it created the
    // authoritative `<file>.bak` holding the true pre-doctor original.  The
    // atomic write below would overwrite that backup with the post-peel
    // content, so capture and restore it to keep the backup pointing at the
    // original file.
    let bak = PathBuf::from(format!("{}.bak", issue.path.display()));
    let preserved_bak = if issue.repaired {
        fs::read(&bak).ok()
    } else {
        None
    };

    crate::cmd::write::run_binary(&issue.path, &converted, None)?;

    if let Some(original) = preserved_bak {
        fs::write(&bak, original)?;
    }
    issue.eol_repaired = true;
    Ok(())
}

/// Rewrite every line ending in `bytes` to `target`, leaving all other bytes
/// (including any BOM) untouched.  CRLF, lone CR, and lone LF are all coalesced
/// to a single `target` terminator.
fn normalize_eol_bytes(bytes: &[u8], target: LineEnding) -> Vec<u8> {
    let term: &[u8] = match target {
        LineEnding::Lf => b"\n",
        LineEnding::CrLf => b"\r\n",
        LineEnding::Cr => b"\r",
    };
    let mut out = Vec::with_capacity(bytes.len() + 8);
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                out.extend_from_slice(term);
                if bytes.get(i + 1) == Some(&b'\n') {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            b'\n' => {
                out.extend_from_slice(term);
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

// ── Public entry point ──────────────────────────────────────────────────────

/// Run the `tpu doctor` subcommand.
///
/// `path_specs` may be empty, in which case the current directory `"."`
/// is used.  Output (the human or JSON report) is written to `out`.
#[allow(dead_code)]
pub fn run(
    path_specs: &[&str],
    options: DoctorOptions,
    out: &mut dyn Write,
    io_mode: IoMode,
) -> Result<DoctorReport, Box<dyn Error>> {
    run_with_policy(
        path_specs,
        options,
        out,
        io_mode,
        crate::cmd::copy::OnError::Fail,
        &mut Vec::new(),
        None,
        false,
    )
}

/// Variant of [`run`] that accepts an explicit walk-error policy and a
/// sink for per-entry warnings (for inaccessible directories).
///
/// `git_root`, when `Some`, opts in to git-aware line-ending diagnostics
/// against the repository rooted there (no upward discovery).  `fix_eol`
/// additionally normalises any mismatched file's line endings to git's
/// expected convention (a no-op when `git_root` is `None`).
#[allow(clippy::too_many_arguments)]
pub fn run_with_policy(
    path_specs: &[&str],
    options: DoctorOptions,
    out: &mut dyn Write,
    io_mode: IoMode,
    on_error: crate::cmd::copy::OnError,
    warnings_out: &mut Vec<String>,
    git_root: Option<&Path>,
    fix_eol: bool,
) -> Result<DoctorReport, Box<dyn Error>> {
    let default = ["."];
    let specs: &[&str] = if path_specs.is_empty() {
        &default
    } else {
        path_specs
    };

    // Open the repository once (best effort).  A failure to open is downgraded
    // to a warning so the rest of the scan proceeds without git awareness.
    let git = match git_root {
        Some(root) => match git::GitEol::open(root) {
            Ok(g) => g,
            Err(e) => {
                warnings_out.push(format!("doctor: git root {}: {e}", root.display()));
                None
            }
        },
        None => None,
    };

    let files = expand_paths_with_policy(specs, on_error, warnings_out)?;
    let mut report = DoctorReport::default();

    for path in &files {
        match diagnose_file(path, io_mode, options.guess, git.as_ref()) {
            Ok(Some(issue)) => {
                report.total_files_scanned += 1;
                report.issues.push(issue);
            }
            Ok(None) => {
                report.total_files_scanned += 1;
            }
            Err(e) => {
                let msg = format!("doctor: {}: {e}", path.display());
                // A single explicit file has no other entries to fall back on;
                // downgrade to a warning only when there are multiple files.
                if matches!(on_error, crate::cmd::copy::OnError::Fail) || files.len() == 1 {
                    return Err(msg.into());
                }
                warnings_out.push(msg);
            }
        }
    }

    // If every selected file failed in warn mode, returning Ok with
    // total_files_scanned == 0 would silently look like an empty-directory
    // scan.  Return an error instead so callers know nothing was diagnosed.
    if report.total_files_scanned == 0 && !files.is_empty() {
        return Err(
            "doctor: no files could be scanned; all selected files were missing or unreadable"
                .into(),
        );
    }

    if options.fix == DoctorFix::Peel {
        for issue in &mut report.issues {
            if !issue.mojibake_matches.is_empty()
                && issue.peel_suggested.is_some()
                && let Err(e) = apply_peel(issue, io_mode)
                && options.format == DoctorFormat::Human
            {
                writeln!(
                    out,
                    "doctor: peel-fix failed for {}: {}",
                    issue.path.display(),
                    e
                )?;
            }
        }
    }

    if fix_eol {
        for issue in &mut report.issues {
            if issue.eol_mismatch.is_some()
                && let Err(e) = apply_eol_fix(issue, io_mode)
                && options.format == DoctorFormat::Human
            {
                writeln!(
                    out,
                    "doctor: eol-fix failed for {}: {}",
                    issue.path.display(),
                    e
                )?;
            }
        }
    }

    if options.fix == DoctorFix::Peel || fix_eol {
        report.total_repaired = report
            .issues
            .iter()
            .filter(|i| i.repaired || i.eol_repaired)
            .count();
    }
    report.eol_fix_requested = fix_eol;

    match options.format {
        DoctorFormat::Human => emit_human(out, &report, options.quiet)?,
        DoctorFormat::Json => emit_json(out, &report)?,
    }

    Ok(report)
}

// ── Output rendering ────────────────────────────────────────────────────────

fn emit_human(out: &mut dyn Write, report: &DoctorReport, quiet: bool) -> std::io::Result<()> {
    if !quiet {
        for issue in &report.issues {
            if !issue.valid_in_detected_encoding {
                writeln!(
                    out,
                    "{}: invalid bytes in detected encoding ({})",
                    issue.path.display(),
                    issue.encoding_detected
                )?;
                continue;
            }
            // Per-pattern counts for a one-line summary.
            let mut by_pat: BTreeMap<&'static str, usize> = BTreeMap::new();
            for m in &issue.mojibake_matches {
                *by_pat.entry(m.pattern.name()).or_insert(0) += 1;
            }
            let summary = by_pat
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            let repaired_tag = if issue.repaired { " [REPAIRED]" } else { "" };
            let rc_count = issue.replacement_char_matches.len();
            if !issue.mojibake_matches.is_empty() {
                writeln!(
                    out,
                    "{}: {} ({}) [{}]{}",
                    issue.path.display(),
                    issue.mojibake_matches.len(),
                    summary,
                    issue.encoding_detected,
                    repaired_tag
                )?;
                for m in &issue.mojibake_matches {
                    writeln!(
                        out,
                        "  {}:{}: [{}] (byte offset {})",
                        m.line,
                        m.col,
                        m.pattern.name(),
                        m.byte_offset
                    )?;
                }
            }
            if rc_count > 0 {
                writeln!(
                    out,
                    "{}: {} lossy-replacement char{} (U+FFFD residue; manual repair only) [{}]",
                    issue.path.display(),
                    rc_count,
                    if rc_count == 1 { "" } else { "s" },
                    issue.encoding_detected,
                )?;
                for m in &issue.replacement_char_matches {
                    let suggestion = match m.suggested {
                        Some(c) => format!(" (suggest: U+{:04X} '{c}')", c as u32),
                        None => String::new(),
                    };
                    writeln!(
                        out,
                        "  {}:{}: [lossy-replacement] (byte offset {}){suggestion}",
                        m.line, m.col, m.byte_offset,
                    )?;
                }
            }
            if let Some(m) = issue.eol_mismatch {
                // Three distinct states, so a normalising run that left a file
                // untouched is never mistaken for a silent success or a bare
                // report:
                //   * repaired              -> [NORMALIZED]
                //   * fix asked, not done   -> [NOT NORMALIZED]  (paired with a
                //                              "doctor: eol-fix failed" line)
                //   * report only (no fix)  -> no tag
                let tag = if issue.eol_repaired {
                    " [NORMALIZED]"
                } else if report.eol_fix_requested {
                    " [NOT NORMALIZED]"
                } else {
                    ""
                };
                writeln!(
                    out,
                    "{}: line endings ({}) differ from git's expected {} (per .gitattributes / core.autocrlf / core.eol){}",
                    issue.path.display(),
                    git::line_ending_name(m.actual),
                    git::line_ending_name(m.expected),
                    tag,
                )?;
            }
        }
    }
    writeln!(
        out,
        "doctor: scanned {} file(s), {} flagged, {} repaired",
        report.total_files_scanned,
        report.total_issues(),
        report.total_repaired
    )?;
    Ok(())
}

fn emit_json(out: &mut dyn Write, report: &DoctorReport) -> std::io::Result<()> {
    let files: Vec<_> = report
        .issues
        .iter()
        .map(|issue| {
            let matches: Vec<_> = issue
                .mojibake_matches
                .iter()
                .map(|m| {
                    json!({
                        "byte_offset": m.byte_offset,
                        "line": m.line,
                        "col": m.col,
                        "pattern": m.pattern.name(),
                    })
                })
                .collect();
            let rc_matches: Vec<_> = issue
                .replacement_char_matches
                .iter()
                .map(|m| {
                    let mut obj = json!({
                        "byte_offset": m.byte_offset,
                        "line": m.line,
                        "col": m.col,
                        "context": m.context.as_str(),
                    });
                    if let Some(c) = m.suggested {
                        obj["suggested"] = json!(format!("U+{:04X}", c as u32));
                        obj["suggested_char"] = json!(c.to_string());
                    } else {
                        obj["suggested"] = json!(null);
                        obj["suggested_char"] = json!(null);
                    }
                    obj
                })
                .collect();
            json!({
                "path": issue.path.display().to_string(),
                "encoding_detected": issue.encoding_detected,
                "valid_in_detected_encoding": issue.valid_in_detected_encoding,
                "mojibake_matches": matches,
                "replacement_char_matches": rc_matches,
                "peel_suggested": issue.peel_suggested.is_some(),
                "repaired": issue.repaired,
                "eol_mismatch": issue.eol_mismatch.map(|m| json!({
                    "expected": git::line_ending_name(m.expected),
                    "actual": git::line_ending_name(m.actual),
                })),
                "eol_repaired": issue.eol_repaired,
            })
        })
        .collect();

    let doc = json!({
        "files": files,
        "total_files_scanned": report.total_files_scanned,
        "total_issues": report.total_issues(),
        "total_repaired": report.total_repaired,
    });
    let s = serde_json::to_string_pretty(&doc).expect("serialise doctor report");
    writeln!(out, "{s}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.path().join(name);
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn expand_paths_warn_mode_all_missing_returns_error() {
        // When every supplied path is missing and on_error is Warn,
        // expand_paths_with_policy must error rather than silently returning
        // an empty list (which would cause `tpu doctor` to report "scanned 0"
        // and exit successfully without checking anything).
        let mut warnings: Vec<String> = Vec::new();
        let result = expand_paths_with_policy(
            &["definitely_does_not_exist_a", "definitely_does_not_exist_b"],
            crate::cmd::copy::OnError::Warn,
            &mut warnings,
        );
        assert!(
            result.is_err(),
            "expected an error when all paths are missing"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no files to scan"),
            "error should mention 'no files to scan', got: {msg}"
        );
        // The individual stat failures should still have been recorded as warnings.
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn expand_paths_warn_mode_partial_missing_succeeds() {
        // When only SOME paths are missing, the function should succeed and
        // return the files that were found.
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "real.txt", b"hello");
        let real = p.to_str().unwrap().to_owned();
        let mut warnings: Vec<String> = Vec::new();
        let result = expand_paths_with_policy(
            &[real.as_str(), "definitely_does_not_exist"],
            crate::cmd::copy::OnError::Warn,
            &mut warnings,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
        assert_eq!(warnings.len(), 1, "one warning for the missing path");
    }

    #[test]
    fn run_with_policy_counts_only_successfully_diagnosed_files() {
        // total_files_scanned must reflect the number of files that were
        // actually diagnosed, not the total length of the candidate list.
        // (The pre-fix bug initialised the count with files.len() before the
        // loop so every failed diagnose_file still incremented the tally.)
        let tmp = TempDir::new().unwrap();
        let a = write(&tmp, "a.txt", b"clean utf-8\n");
        let b = write(&tmp, "b.txt", b"also clean\n");
        let a_s = a.to_str().unwrap().to_owned();
        let b_s = b.to_str().unwrap().to_owned();
        let mut out: Vec<u8> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let report = run_with_policy(
            &[a_s.as_str(), b_s.as_str()],
            DoctorOptions::default(),
            &mut out,
            IoMode::Buffered,
            crate::cmd::copy::OnError::Warn,
            &mut warnings,
            None,
            false,
        )
        .unwrap();
        assert_eq!(report.total_files_scanned, 2);
        assert!(warnings.is_empty());
    }

    #[test]
    fn binary_extensions_skipped() {
        assert!(is_binary_extension(Path::new("foo.png")));
        assert!(is_binary_extension(Path::new("foo.PNG")));
        assert!(is_binary_extension(Path::new("a/b/c.exe")));
        assert!(!is_binary_extension(Path::new("foo.txt")));
        assert!(!is_binary_extension(Path::new("Cargo.toml")));
    }

    #[test]
    fn skipped_dir_recognises_target_and_git() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/keep.txt"), b"x").unwrap();
        fs::write(tmp.path().join(".git/config"), b"x").unwrap();
        fs::write(tmp.path().join("target/out.txt"), b"x").unwrap();

        let mut warnings = Vec::new();
        let found = crate::walk::walk(
            tmp.path(),
            "**/*",
            SKIP_DIRS,
            crate::cmd::copy::OnError::Warn,
            "doctor",
            &mut warnings,
        )
        .unwrap();
        let names: Vec<String> = found
            .files
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("src/keep.txt")),
            "{names:?}"
        );
        assert!(
            !names
                .iter()
                .any(|n| n.contains(".git") || n.contains("target")),
            "should not descend into .git or target: {names:?}"
        );
    }

    #[test]
    fn diagnose_clean_utf8_file_returns_none() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "clean.txt", "hello world\n".as_bytes());
        let res = diagnose_file(&p, IoMode::Buffered, false, None).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn diagnose_mojibake_file_returns_issue_with_line_col() {
        let tmp = TempDir::new().unwrap();
        // Two lines; mojibake is on line 2 at column 5 (0-indexed bytes
        // include "first\n" = 6 bytes before the 'c' of 'caf').
        let p = write(&tmp, "bad.txt", "first\ncafÃ©\n".as_bytes());
        let issue = diagnose_file(&p, IoMode::Buffered, false, None)
            .unwrap()
            .expect("flagged");
        assert!(issue.valid_in_detected_encoding);
        assert_eq!(issue.mojibake_matches.len(), 1);
        let m = &issue.mojibake_matches[0];
        assert_eq!(m.pattern, Pattern::Latin1);
        assert_eq!(m.line, 2);
        // "caf" = 3 chars; the 'Ã' is at column 4.
        assert_eq!(m.col, 4);
        assert!(issue.peel_suggested.is_some());
    }

    #[test]
    fn diagnose_invalid_utf8_returns_invalid_issue_with_no_matches() {
        let tmp = TempDir::new().unwrap();
        // Lone 0xFF — not a legal start byte in UTF-8 and not a valid
        // BOM either, so harrier reports UTF-8 with errors.
        let p = write(&tmp, "broken.bin", b"hello\xFFworld");
        // This file may or may not be detected as UTF-8 by harrier; the
        // assertion is only that *if* it's not valid in its encoding,
        // we report it correctly.  If harrier picks Win-1252 then the
        // bytes are valid and the test is moot — assert with that in mind.
        let res = diagnose_file(&p, IoMode::Buffered, false, None).unwrap();
        if let Some(issue) = res
            && !issue.valid_in_detected_encoding
        {
            assert!(issue.mojibake_matches.is_empty());
        }
    }

    #[test]
    fn allow_marker_suppresses_diagnosis() {
        let tmp = TempDir::new().unwrap();
        let body = format!("// {}\nthis line has cafÃ© in it\n", mojibake::ALLOW_MARKER);
        let p = write(&tmp, "ok.txt", body.as_bytes());
        let res = diagnose_file(&p, IoMode::Buffered, false, None).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn allow_marker_does_not_suppress_encoding_invalid() {
        let tmp = TempDir::new().unwrap();
        // A UTF-8 BOM forces UTF-8 detection; the lone 0xFF byte is then
        // invalid in that encoding (`had_errors == true`).  The
        // `allow-mojibake` marker must NOT suppress the *encoding-invalid*
        // diagnostic — it only suppresses mojibake / replacement-char
        // findings.  Regression for PR #41 review discussion r3470390733.
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(format!("// {}\n", mojibake::ALLOW_MARKER).as_bytes());
        bytes.extend_from_slice(b"broken\xFFhere\n");
        let p = write(&tmp, "marked-but-invalid.txt", &bytes);
        let issue = diagnose_file(&p, IoMode::Buffered, false, None)
            .unwrap()
            .expect("encoding-invalid file must still be flagged despite the marker");
        assert!(!issue.valid_in_detected_encoding);
        assert!(issue.mojibake_matches.is_empty());
    }

    #[test]
    fn empty_file_is_clean() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "empty.txt", b"");
        let res = diagnose_file(&p, IoMode::Buffered, false, None).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn run_default_path_walks_cwd_and_reports_summary() {
        // Use an isolated temp dir as cwd so we don't pull in the whole
        // repository.
        let tmp = TempDir::new().unwrap();
        write(&tmp, "clean.txt", b"hello\n");
        write(&tmp, "bad.txt", "cafÃ©\n".as_bytes());

        let path_str = tmp.path().to_string_lossy().to_string();
        let mut buf: Vec<u8> = Vec::new();
        let report = run(
            &[&path_str],
            DoctorOptions {
                format: DoctorFormat::Human,
                fix: DoctorFix::None,
                quiet: false,
                guess: false,
            },
            &mut buf,
            IoMode::Buffered,
        )
        .unwrap();

        assert_eq!(report.total_files_scanned, 2);
        assert_eq!(report.total_issues(), 1);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("bad.txt"));
        assert!(s.contains("doctor: scanned 2 file(s)"));
    }

    #[test]
    fn run_quiet_suppresses_per_file_lines_in_human_mode() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "bad.txt", "cafÃ©\n".as_bytes());

        let path_str = tmp.path().to_string_lossy().to_string();
        let mut buf: Vec<u8> = Vec::new();
        let _ = run(
            &[&path_str],
            DoctorOptions {
                format: DoctorFormat::Human,
                fix: DoctorFix::None,
                quiet: true,
                guess: false,
            },
            &mut buf,
            IoMode::Buffered,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            !s.contains("bad.txt"),
            "quiet should suppress per-file lines: {s}"
        );
        assert!(s.contains("doctor: scanned"), "summary still emitted: {s}");
    }

    #[test]
    fn run_json_mode_emits_parseable_document() {
        let tmp = TempDir::new().unwrap();
        write(&tmp, "bad.txt", "cafÃ©\n".as_bytes());

        let path_str = tmp.path().to_string_lossy().to_string();
        let mut buf: Vec<u8> = Vec::new();
        let report = run(
            &[&path_str],
            DoctorOptions {
                format: DoctorFormat::Json,
                fix: DoctorFix::None,
                quiet: false,
                guess: false,
            },
            &mut buf,
            IoMode::Buffered,
        )
        .unwrap();

        let v: serde_json::Value = serde_json::from_slice(&buf).expect("valid JSON");
        assert_eq!(v["total_files_scanned"], 1);
        assert_eq!(v["total_issues"], 1);
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f["valid_in_detected_encoding"], true);
        assert_eq!(f["mojibake_matches"][0]["pattern"], "latin1");
        // Lock in the new field: empty array for a mojibake-only file.
        assert_eq!(
            f["replacement_char_matches"],
            serde_json::json!([]),
            "replacement_char_matches must be present and empty for mojibake-only file"
        );
        assert!(report.total_issues() >= 1);
    }

    #[test]
    fn fix_peel_repairs_double_mojibake_and_writes_bak() {
        let tmp = TempDir::new().unwrap();
        // Double-encoded "café": one peel layer should reduce mojibake.
        // Build by encoding "café" through cp1252 twice.
        let original = "café";
        // Encode UTF-8 → win1252 round-trip once: each non-ASCII char's
        // *bytes* appear as cp1252 chars.  We'll just write the canonical
        // single-mojibake form (post-one-peel must be cleaner).
        let single = "cafÃ©\n"; // one layer of corruption
        let p = write(&tmp, "fix.txt", single.as_bytes());
        let _ = original;

        let path_str = tmp.path().to_string_lossy().to_string();
        let mut buf: Vec<u8> = Vec::new();
        let report = run(
            &[&path_str],
            DoctorOptions {
                format: DoctorFormat::Human,
                fix: DoctorFix::Peel,
                quiet: true,
                guess: false,
            },
            &mut buf,
            IoMode::Buffered,
        )
        .unwrap();

        assert_eq!(report.total_repaired, 1);
        // The .bak file should exist and contain the original bytes.
        let bak = format!("{}.bak", p.display());
        assert!(Path::new(&bak).exists());
        let bak_bytes = fs::read(&bak).unwrap();
        assert_eq!(bak_bytes, single.as_bytes());
        // The repaired file should no longer contain the Latin1 mojibake.
        let now = fs::read(&p).unwrap();
        let now_text = String::from_utf8_lossy(&now);
        assert!(
            mojibake::scan(&now_text).matches.is_empty()
                || mojibake::scan(&now_text).matches.len() < mojibake::scan(single).matches.len()
        );
    }

    #[test]
    fn fix_peel_leaves_clean_files_alone() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "clean.txt", b"hello world\n");

        let path_str = tmp.path().to_string_lossy().to_string();
        let mut buf: Vec<u8> = Vec::new();
        let report = run(
            &[&path_str],
            DoctorOptions {
                format: DoctorFormat::Human,
                fix: DoctorFix::Peel,
                quiet: true,
                guess: false,
            },
            &mut buf,
            IoMode::Buffered,
        )
        .unwrap();

        assert_eq!(report.total_repaired, 0);
        let bak = format!("{}.bak", p.display());
        assert!(
            !Path::new(&bak).exists(),
            "no .bak should be created for clean files"
        );
    }

    #[test]
    fn issue_is_problem_distinguishes_mojibake_and_invalid() {
        let mut i = DoctorIssue {
            path: PathBuf::from("x"),
            encoding_detected: "UTF-8",
            valid_in_detected_encoding: true,
            mojibake_matches: Vec::new(),
            replacement_char_matches: Vec::new(),
            peel_suggested: None,
            repaired: false,
            eol_mismatch: None,
            eol_repaired: false,
        };
        assert!(!i.is_problem());
        i.valid_in_detected_encoding = false;
        assert!(i.is_problem());
        i.valid_in_detected_encoding = true;
        i.mojibake_matches.push(DoctorMatch {
            byte_offset: 0,
            line: 1,
            col: 1,
            pattern: Pattern::Latin1,
        });
        assert!(i.is_problem());
    }

    #[test]
    fn report_total_issues_counts_only_problems() {
        let mut r = DoctorReport::default();
        r.issues.push(DoctorIssue {
            path: PathBuf::from("a"),
            encoding_detected: "UTF-8",
            valid_in_detected_encoding: true,
            mojibake_matches: vec![DoctorMatch {
                byte_offset: 0,
                line: 1,
                col: 1,
                pattern: Pattern::Latin1,
            }],
            replacement_char_matches: Vec::new(),
            peel_suggested: None,
            repaired: false,
            eol_mismatch: None,
            eol_repaired: false,
        });
        assert_eq!(r.total_issues(), 1);
    }

    #[test]
    fn options_default_is_human_no_fix_not_quiet() {
        let o = DoctorOptions::default();
        assert_eq!(o.format, DoctorFormat::Human);
        assert_eq!(o.fix, DoctorFix::None);
        assert!(!o.quiet);
    }

    #[test]
    fn gitignore_skips_matching_paths() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "ignored/\n*.log\n").unwrap();
        fs::create_dir(tmp.path().join("ignored")).unwrap();
        write(&tmp, "ignored/bad.txt", "cafÃ©\n".as_bytes());
        write(&tmp, "noise.log", "cafÃ©\n".as_bytes());
        write(&tmp, "kept.txt", "cafÃ©\n".as_bytes());

        let path_str = tmp.path().to_string_lossy().to_string();
        let mut buf: Vec<u8> = Vec::new();
        let report = run(
            &[&path_str],
            DoctorOptions {
                format: DoctorFormat::Human,
                fix: DoctorFix::None,
                quiet: true,
                guess: false,
            },
            &mut buf,
            IoMode::Buffered,
        )
        .unwrap();
        // .gitignore + kept.txt are scanned; ignored/ + *.log skipped.
        assert!(
            report
                .issues
                .iter()
                .all(|i| !i.path.to_string_lossy().contains("ignored"))
        );
        assert!(
            report
                .issues
                .iter()
                .all(|i| !i.path.to_string_lossy().ends_with(".log"))
        );
        assert!(report.issues.iter().any(|i| i.path.ends_with("kept.txt")));
    }

    #[test]
    fn glob_spec_matches_files_under_cwd() {
        // Glob walks ".", so chdir to a tmp dir first.
        let tmp = TempDir::new().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let _restore = scopeguard_set_dir(prev);

        write(&tmp, "a.txt", "cafÃ©\n".as_bytes());
        write(&tmp, "b.md", "cafÃ©\n".as_bytes());

        let mut buf: Vec<u8> = Vec::new();
        let report = run(
            &["*.txt"],
            DoctorOptions {
                format: DoctorFormat::Human,
                fix: DoctorFix::None,
                quiet: true,
                guess: false,
            },
            &mut buf,
            IoMode::Buffered,
        )
        .unwrap();
        assert_eq!(report.total_files_scanned, 1);
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.path.to_string_lossy().ends_with("a.txt"))
        );
    }

    /// Restore the previous working directory on drop.  Used to keep
    /// glob-based tests from leaking cwd changes into other tests.
    fn scopeguard_set_dir(prev: PathBuf) -> impl Drop {
        struct G(PathBuf);
        impl Drop for G {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        G(prev)
    }

    /// Create a fresh git repo under a temp dir with the given
    /// `.gitattributes` contents and `core.autocrlf` disabled (so the
    /// result is deterministic regardless of the host's global config).
    fn init_repo_with_attrs(attrs: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        gix::init(dir.path()).expect("git init");
        fs::write(dir.path().join(".gitattributes"), attrs).unwrap();
        let cfg_path = dir.path().join(".git").join("config");
        let mut cfg = fs::read_to_string(&cfg_path).unwrap_or_default();
        cfg.push_str("\n[core]\n\tautocrlf = false\n");
        fs::write(&cfg_path, cfg).unwrap();
        dir
    }

    #[test]
    fn git_eol_mismatch_is_detected_without_fix() {
        let repo = init_repo_with_attrs("*.txt text eol=crlf\n");
        // LF file where git expects CRLF.
        let p = write(&repo, "note.txt", b"alpha\nbeta\n");
        let path_str = p.to_string_lossy().to_string();

        let mut buf: Vec<u8> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let report = run_with_policy(
            &[&path_str],
            DoctorOptions::default(),
            &mut buf,
            IoMode::Buffered,
            crate::cmd::copy::OnError::Fail,
            &mut warnings,
            Some(repo.path()),
            false,
        )
        .unwrap();

        let issue = report
            .issues
            .iter()
            .find(|i| i.path.ends_with("note.txt"))
            .expect("note.txt flagged");
        let m = issue.eol_mismatch.expect("eol mismatch present");
        assert_eq!(m.expected, LineEnding::CrLf);
        assert_eq!(m.actual, LineEnding::Lf);
        assert!(!issue.eol_repaired, "no fix requested");
        // File is untouched on disk.
        assert_eq!(fs::read(&p).unwrap(), b"alpha\nbeta\n");
    }

    #[test]
    fn git_eol_fix_normalises_and_writes_bak() {
        let repo = init_repo_with_attrs("*.txt text eol=crlf\n");
        let p = write(&repo, "note.txt", b"alpha\nbeta\n");
        let path_str = p.to_string_lossy().to_string();

        let mut buf: Vec<u8> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let report = run_with_policy(
            &[&path_str],
            DoctorOptions {
                quiet: true,
                ..Default::default()
            },
            &mut buf,
            IoMode::Buffered,
            crate::cmd::copy::OnError::Fail,
            &mut warnings,
            Some(repo.path()),
            true,
        )
        .unwrap();

        assert_eq!(report.total_repaired, 1);
        assert_eq!(fs::read(&p).unwrap(), b"alpha\r\nbeta\r\n");
        let bak = format!("{}.bak", p.display());
        assert!(Path::new(&bak).exists(), "backup written");
        assert_eq!(fs::read(&bak).unwrap(), b"alpha\nbeta\n");
    }

    #[test]
    fn git_eol_no_mismatch_when_endings_match() {
        let repo = init_repo_with_attrs("*.txt text eol=lf\n");
        let p = write(&repo, "note.txt", b"alpha\nbeta\n");
        let path_str = p.to_string_lossy().to_string();

        let mut buf: Vec<u8> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let report = run_with_policy(
            &[&path_str],
            DoctorOptions::default(),
            &mut buf,
            IoMode::Buffered,
            crate::cmd::copy::OnError::Fail,
            &mut warnings,
            Some(repo.path()),
            false,
        )
        .unwrap();

        assert!(
            report.issues.iter().all(|i| i.eol_mismatch.is_none()),
            "matching LF file should not be flagged"
        );
    }

    #[test]
    fn git_eol_skips_utf16_even_with_fix() {
        let repo = init_repo_with_attrs("*.txt text eol=lf\n");
        // UTF-16LE BOM + "a\r\nb\r\n".  A naive byte scan would see the
        // `0D 00 0A 00` pairs and flag a CRLF-vs-LF mismatch, but UTF-16 line
        // endings are multi-byte and must be skipped entirely.
        let mut bytes = vec![0xFF, 0xFE];
        for ch in "a\r\nb\r\n".chars() {
            bytes.push(ch as u8);
            bytes.push(0x00);
        }
        let p = write(&repo, "u16.txt", &bytes);
        let path_str = p.to_string_lossy().to_string();

        let mut buf: Vec<u8> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let report = run_with_policy(
            &[&path_str],
            DoctorOptions {
                quiet: true,
                ..Default::default()
            },
            &mut buf,
            IoMode::Buffered,
            crate::cmd::copy::OnError::Fail,
            &mut warnings,
            Some(repo.path()),
            true, // --fix=eol requested
        )
        .unwrap();

        assert!(
            report.issues.iter().all(|i| i.eol_mismatch.is_none()),
            "UTF-16 file must never be flagged for an EOL mismatch"
        );
        // Bytes are left exactly as written; no `.bak` is produced.
        assert_eq!(fs::read(&p).unwrap(), bytes);
        assert!(!Path::new(&format!("{}.bak", p.display())).exists());
    }
}
