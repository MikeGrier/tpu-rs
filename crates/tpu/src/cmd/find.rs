// Copyright (c) 2026, Michael Grier

//! `tpu find` — encoding-aware pattern search across one or more files.
//!
//! Extends the algorithm from `search.rs` to support multiple patterns
//! (OR/AND), multiple files selected by path or glob pattern, configurable
//! file-prefixed output, and an explicit `--invert` flag.
//!
//! # Output format
//!
//! | Situation                             | Format                        |
//! |---------------------------------------|-------------------------------|
//! | Single path, no `--numbers`           | `<text>`                      |
//! | Single path, `--numbers`              | `<lineno>:<text>`             |
//! | Multi-path, no `--numbers`            | `<file>:<text>`               |
//! | Multi-path, `--numbers`               | `<file>:<lineno>:<text>`      |
//! | Context lines (single path)           | `<lineno>-<text>`             |
//! | Context lines (multi-path)            | `<file>-<lineno>-<text>`      |
//! | Group separator (with `-A`/`-B`)      | `--`                          |
//! | Count mode, single file              | `<N>`                         |
//! | Count mode, per file (multi)          | `<file>: <N>`                 |
//! | Count mode, total (multi, >1 file)    | `total: <N>`                  |
//!
//! # Exit codes (applied by the caller)
//!
//! - **0** — ≥1 match found (or `--count` mode completed without error)
//! - **1** — no matches
//! - **2** — bad glob, invalid regex, I/O failure, or missing arguments

use std::{
    collections::VecDeque,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use globset::Glob;
use harrier::{encoding::SourceConfig, lines::LineTerminator, source::Source};
use regex::Regex;
use walkdir::WalkDir;

use crate::IoMode;

// ── Public result type ────────────────────────────────────────────────────────

/// Result returned by [`run`].
#[derive(Debug)]
pub struct FindResult {
    /// Total number of matching lines across all files searched.
    pub total_matches: usize,
}

// ── Pattern compilation ───────────────────────────────────────────────────────

/// Compile one `Regex` per pattern string, applying `fixed_string` and
/// `multiline` transformations.
///
/// `fixed_string = true` applies [`regex::escape`] so every metacharacter is
/// literal.  `multiline = true` prepends `(?m)` so `^`/`$` match at LF
/// boundaries within each decoded line.  `ignore_case = true` prepends `(?i)`
/// for case-insensitive matching.
fn build_patterns(
    patterns: &[&str],
    fixed_string: bool,
    multiline: bool,
    ignore_case: bool,
) -> Result<Vec<Regex>, Box<dyn std::error::Error>> {
    patterns
        .iter()
        .map(|p| {
            let escaped = if fixed_string {
                regex::escape(p)
            } else {
                p.to_string()
            };
            // Build the flag prefix: flags are composable inline modifiers.
            let flags = match (ignore_case, multiline) {
                (true, true) => "(?im)",
                (true, false) => "(?i)",
                (false, true) => "(?m)",
                (false, false) => "",
            };
            let effective = format!("{flags}{escaped}");
            Regex::new(&effective).map_err(|e| format!("find: invalid pattern {:?}: {e}", p).into())
        })
        .collect()
}

// ── Predicate evaluation ──────────────────────────────────────────────────────

/// Returns `true` when `line` satisfies the composite predicate formed from
/// `regexes`, `all_match`, and `invert`.
///
/// | `all_match` | `invert` | Passes when…                              |
/// |-------------|----------|------------------------------------------|
/// | false       | false    | any regex matches                         |
/// | false       | true     | no regex matches                          |
/// | true        | false    | every regex matches                       |
/// | true        | true     | at least one regex fails to match         |
fn line_matches(line: &str, regexes: &[Regex], all_match: bool, invert: bool) -> bool {
    let raw = if all_match {
        regexes.iter().all(|re| re.is_match(line))
    } else {
        regexes.iter().any(|re| re.is_match(line))
    };
    if invert { !raw } else { raw }
}

// ── Glob / path expansion ─────────────────────────────────────────────────────

/// Expand path specifications (bare file paths or glob patterns) into
/// concrete file paths.
///
/// A spec is treated as a glob when it contains `*`, `?`, `[`, or `{`.
/// Otherwise it is used as a literal path.
///
/// Returns an error if a glob pattern is syntactically invalid or matches no
/// files.
///
/// Equivalent to [`expand_paths_with_policy`] with no `glob` and
/// [`crate::cmd::copy::OnError::Fail`] — i.e. the legacy "abort on first walk
/// error" behaviour.
#[allow(dead_code)]
pub fn expand_paths(path_specs: &[&str]) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    expand_paths_with_policy(
        path_specs,
        None,
        crate::cmd::copy::OnError::Fail,
        &mut Vec::new(),
    )
}

/// Expand path specifications with explicit handling of walk errors.
///
/// When `glob` is `Some`, each directory in `path_specs` is walked
/// recursively and every file whose path-relative-to-that-directory matches
/// the supplied glob is included. File specs are included as-is (the caller
/// asked for that exact file). Path specs that themselves contain glob
/// metacharacters are rejected in this mode — pick one form.
///
/// When `on_error` is [`crate::cmd::copy::OnError::Warn`] (the default for
/// CLI/MCP use), unreadable directories produce a textual warning appended
/// to `warnings_out` instead of aborting the whole expansion. The caller
/// can then route those warnings to its [`crate::shell::Shell`] (NDJSON
/// `{"reason":"warning"}` records or stderr notes).
pub fn expand_paths_with_policy(
    path_specs: &[&str],
    glob: Option<&str>,
    on_error: crate::cmd::copy::OnError,
    warnings_out: &mut Vec<String>,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut paths: Vec<PathBuf> = Vec::new();

    // Pre-compile the filename matcher once when `glob` is supplied so we
    // don't recompile per spec.
    let glob_matcher = match glob {
        Some(g) => Some(
            Glob::new(g)
                .map_err(|e| format!("find: invalid glob {:?}: {e}", g))?
                .compile_matcher(),
        ),
        None => None,
    };

    for &spec in path_specs {
        let is_glob =
            spec.contains('*') || spec.contains('?') || spec.contains('[') || spec.contains('{');

        if let Some(ref matcher) = glob_matcher {
            // `glob` mode: every spec is either a directory (walked and
            // filtered by the glob) or a literal file (included as-is).
            // Mixing in a glob-shaped path spec is ambiguous, so reject.
            if is_glob {
                return Err(format!(
                    "find: path {:?} contains glob metacharacters but a \
                     separate `glob` was also supplied; pick one form",
                    spec,
                )
                .into());
            }
            let p = PathBuf::from(spec);
            if p.is_dir() {
                let before = paths.len();
                for entry in WalkDir::new(&p) {
                    let entry = match entry {
                        Ok(e) => e,
                        Err(e) => match on_error {
                            crate::cmd::copy::OnError::Fail => {
                                return Err(format!("find: walk error in {spec:?}: {e}").into());
                            }
                            crate::cmd::copy::OnError::Warn => {
                                let path_hint = e
                                    .path()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| "?".to_string());
                                warnings_out
                                    .push(format!("find: cannot access {path_hint}: {e}"));
                                continue;
                            }
                        },
                    };
                    if entry.file_type().is_file() {
                        // Match the path relative to the walk root so the
                        // glob is anchored at the user-supplied directory,
                        // not at CWD.
                        let rel = entry.path().strip_prefix(&p).unwrap_or(entry.path());
                        if matcher.is_match(rel) {
                            paths.push(entry.path().to_path_buf());
                        }
                    }
                }
                if paths.len() == before {
                    return Err(format!(
                        "find: glob {:?} matched no files under {:?}",
                        glob.unwrap(),
                        spec,
                    )
                    .into());
                }
            } else {
                // Literal file path: include as-is. The caller explicitly
                // picked this file, so the glob does not filter it out.
                paths.push(p);
            }
            continue;
        }

        // ── Legacy mode (no separate `glob`) ──────────────────────────────
        if is_glob {
            let matcher = Glob::new(spec)
                .map_err(|e| format!("find: invalid glob {:?}: {e}", spec))?
                .compile_matcher();
            let before = paths.len();
            for entry in WalkDir::new(".") {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => match on_error {
                        crate::cmd::copy::OnError::Fail => {
                            return Err(format!("find: glob walk error: {e}").into());
                        }
                        crate::cmd::copy::OnError::Warn => {
                            let path_hint = e
                                .path()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "?".to_string());
                            warnings_out.push(format!("find: cannot access {path_hint}: {e}"));
                            continue;
                        }
                    },
                };
                if entry.file_type().is_file() {
                    let rel = entry.path().strip_prefix(".").unwrap_or(entry.path());
                    if matcher.is_match(rel) {
                        paths.push(entry.path().to_path_buf());
                    }
                }
            }
            if paths.len() == before {
                return Err(format!("find: glob {:?} matched no files", spec).into());
            }
        } else {
            let p = PathBuf::from(spec);
            if p.is_dir() {
                if p.is_absolute() {
                    // The legacy glob walker starts at "." and only matches
                    // relative paths, so an absolute glob would never match
                    // anything. Point the caller at the `glob` parameter,
                    // which walks the supplied directory directly.
                    return Err(format!(
                        "find: {:?} is a directory — pass a `glob` (e.g. \
                         glob:\"**/*.txt\") to search it recursively, or \
                         pass individual file paths instead",
                        spec,
                    )
                    .into());
                }
                // Normalize the example: strip a leading "./" or ".\\" and any
                // trailing separators so the suggested glob matches what the
                // walker sees after strip_prefix(".").
                let normalized = spec
                    .strip_prefix("./")
                    .or_else(|| spec.strip_prefix(".\\"))
                    .unwrap_or(spec)
                    .trim_end_matches(['/', '\\']);
                // Guard: when the user passes "." or "./" (the walker root),
                // `normalized` is empty or a bare "." — suggest "**" to match
                // all files.  (The walker strips the leading "." from paths
                // before matching, so "./**" would never match anything.)
                let example = if normalized.is_empty() || normalized == "." {
                    "**".to_string()
                } else {
                    format!("{normalized}/**")
                };
                return Err(format!(
                    "find: {:?} is a directory — pass a `glob` (e.g. \
                     glob:\"**/*.txt\") to search it recursively, or pass \
                     a path-glob like {:?}",
                    spec, example,
                )
                .into());
            }
            paths.push(p);
        }
    }
    Ok(paths)
}

// ── Output helpers ────────────────────────────────────────────────────────────

/// Emit one match or context line with the appropriate prefix format.
///
/// `file_prefix` is `Some(name)` in multi-file mode and `None` in single-file
/// mode.  `line_no` is `Some(n)` when the line number should be included —
/// always for context lines, for match lines only when `--numbers` is active.
/// `is_context` selects the `-` separator; match lines use `:`.
fn emit_line(
    out: &mut dyn Write,
    file_prefix: Option<&str>,
    line_no: Option<usize>,
    text: &str,
    is_context: bool,
) -> std::io::Result<()> {
    let sep = if is_context { '-' } else { ':' };
    match (file_prefix, line_no) {
        (Some(f), Some(n)) => writeln!(out, "{f}{sep}{n}{sep}{text}"),
        (Some(f), None) => writeln!(out, "{f}{sep}{text}"),
        (None, Some(n)) => writeln!(out, "{n}{sep}{text}"),
        (None, None) => writeln!(out, "{text}"),
    }
}

/// Emit a count line for one file.
///
/// Single-file mode (`file_prefix = None`): `N\n`.
/// Multi-file mode (`file_prefix = Some(name)`): `<name>: N\n`.
fn emit_count(out: &mut dyn Write, file_prefix: Option<&str>, count: usize) -> std::io::Result<()> {
    match file_prefix {
        Some(f) => writeln!(out, "{f}: {count}"),
        None => writeln!(out, "{count}"),
    }
}

// ── Per-file search ───────────────────────────────────────────────────────────

/// Search a single file with the already-compiled regexes.
///
/// `file_prefix` is `Some(display_name)` in multi-file mode (output is
/// prefixed with the name) and `None` in single-file mode.
///
/// Returns the number of matching lines in this file.
///
/// # Context de-duplication
///
/// A line already emitted as after-context for one match is never re-emitted
/// as before-context for the next match.  The guard is `last_output_num`.
#[allow(clippy::too_many_arguments)]
fn run_single_file(
    file: &Path,
    regexes: &[Regex],
    all_match: bool,
    invert: bool,
    lines_before: usize,
    lines_after: usize,
    count_only: bool,
    numbers: bool,
    file_prefix: Option<&str>,
    out: &mut dyn Write,
    io_mode: IoMode,
) -> Result<usize, Box<dyn std::error::Error>> {
    // Guard against empty-file mmap errors (platform behaviour on Windows).
    let f = fs::File::open(file)?;
    let file_len = f.metadata()?.len();
    if file_len == 0 {
        if count_only {
            emit_count(out, file_prefix, 0)?;
        }
        return Ok(0);
    }
    drop(f);

    let branch = crate::open_as_branch(file, io_mode)?;
    let source = Source::new(Arc::clone(&branch), SourceConfig::default())?;
    let mut iter = source.as_lines()?;
    let encoding = iter.encoding();

    // Ring buffer for before-context: at most `lines_before` entries.
    // Even lines emitted as matches or after-context are pushed so the
    // de-duplication guard can suppress re-emission.
    let mut before_buf: VecDeque<(usize, String)> =
        VecDeque::with_capacity(lines_before.saturating_add(1));

    let mut match_count = 0usize;
    let mut line_num = 0usize;
    let mut after_remaining = 0usize;

    // 1-based number of the last line written to `out`, or None when nothing
    // has been written yet.  Used for separator logic and de-duplication.
    let mut last_output_num: Option<usize> = None;

    loop {
        let (line_bytes, terminator) = match iter.next() {
            None => break,
            Some(item) => item,
        };
        line_num += 1;

        // Decode from source encoding to UTF-8 and strip the normalised LF
        // terminator so the match target is the bare line content.
        let (cow, _had_errors) = encoding.decode_without_bom_handling(&line_bytes);
        let text: String = match terminator {
            LineTerminator::Ending(_) => cow.strip_suffix('\n').unwrap_or(&cow).to_owned(),
            LineTerminator::End => cow.into_owned(),
        };

        let is_match = line_matches(&text, regexes, all_match, invert);

        if is_match {
            match_count += 1;

            if !count_only {
                // ── separator ─────────────────────────────────────────────
                // Emit `--` between non-adjacent output groups when context
                // lines are requested (grep convention: no separator when
                // no context is configured).
                if lines_before > 0 || lines_after > 0 {
                    let first_to_emit = before_buf
                        .iter()
                        .find(|(n, _)| last_output_num.is_none_or(|last| *n > last))
                        .map(|(n, _)| *n)
                        .unwrap_or(line_num);

                    if let Some(last) = last_output_num
                        && first_to_emit > last + 1
                    {
                        writeln!(out, "--")?;
                    }
                }

                // ── before-context ─────────────────────────────────────────
                // Skip any lines already emitted (de-duplication via
                // last_output_num).
                for (n, t) in &before_buf {
                    if last_output_num.is_none_or(|last| *n > last) {
                        emit_line(out, file_prefix, Some(*n), t, true)?;
                        last_output_num = Some(*n);
                    }
                }

                // ── hit line ───────────────────────────────────────────────
                let line_no = if numbers { Some(line_num) } else { None };
                emit_line(out, file_prefix, line_no, &text, false)?;
                last_output_num = Some(line_num);
                after_remaining = lines_after;
            }
        } else if after_remaining > 0 && !count_only {
            // ── after-context ─────────────────────────────────────────────
            after_remaining -= 1;
            emit_line(out, file_prefix, Some(line_num), &text, true)?;
            last_output_num = Some(line_num);
        }

        // Maintain the sliding before-context window.  Every line (including
        // those already emitted as matches or after-context) is pushed so the
        // de-duplication check above can suppress re-emission.
        if !count_only && lines_before > 0 {
            if before_buf.len() == lines_before {
                before_buf.pop_front();
            }
            before_buf.push_back((line_num, text));
        }
    }

    if count_only {
        emit_count(out, file_prefix, match_count)?;
    }

    Ok(match_count)
}

// ── Public entry point ────────────────────────────────────────────────────────

/// Run the `find` subcommand.
///
/// Expands globs in `path_specs`, compiles `patterns` into regexes, and
/// searches each resolved file.
///
/// The caller maps the returned [`FindResult`] to an exit code:
/// `total_matches > 0` → 0, `total_matches == 0` → 1, `Err` → 2.
#[allow(clippy::too_many_arguments, dead_code)]
pub fn run(
    path_specs: &[&str],
    patterns: &[&str],
    glob: Option<&str>,
    fixed_string: bool,
    multiline: bool,
    ignore_case: bool,
    all_match: bool,
    invert: bool,
    lines_before: usize,
    lines_after: usize,
    count_only: bool,
    numbers: bool,
    out: &mut dyn Write,
    io_mode: IoMode,
) -> Result<FindResult, Box<dyn std::error::Error>> {
    run_with_policy(
        path_specs,
        patterns,
        glob,
        fixed_string,
        multiline,
        ignore_case,
        all_match,
        invert,
        lines_before,
        lines_after,
        count_only,
        numbers,
        out,
        io_mode,
        crate::cmd::copy::OnError::Fail,
        &mut Vec::new(),
    )
}

/// Variant of [`run`] that accepts an explicit walk-error policy and a
/// sink for collected per-entry warnings. Use this from the CLI / MCP
/// dispatch so inaccessible directories append warning records to
/// `warnings_out` instead of aborting the entire find.
#[allow(clippy::too_many_arguments)]
pub fn run_with_policy(
    path_specs: &[&str],
    patterns: &[&str],
    glob: Option<&str>,
    fixed_string: bool,
    multiline: bool,
    ignore_case: bool,
    all_match: bool,
    invert: bool,
    lines_before: usize,
    lines_after: usize,
    count_only: bool,
    numbers: bool,
    out: &mut dyn Write,
    io_mode: IoMode,
    on_error: crate::cmd::copy::OnError,
    warnings_out: &mut Vec<String>,
) -> Result<FindResult, Box<dyn std::error::Error>> {
    let regexes = build_patterns(patterns, fixed_string, multiline, ignore_case)?;
    let files = expand_paths_with_policy(path_specs, glob, on_error, warnings_out)?;
    let multi_file = files.len() > 1;

    let mut total_matches = 0usize;
    let mut files_ok = 0usize;
    for file in &files {
        let prefix: Option<String> = if multi_file {
            Some(file.display().to_string())
        } else {
            None
        };
        let result = run_single_file(
            file,
            &regexes,
            all_match,
            invert,
            lines_before,
            lines_after,
            count_only,
            numbers,
            prefix.as_deref(),
            out,
            io_mode,
        );
        match result {
            Ok(n) => {
                total_matches += n;
                files_ok += 1;
            }
            Err(e) => {
                let msg = format!("find: cannot search {}: {e}", file.display());
                // A single explicit file has no other entries to fall back on;
                // downgrade to a warning only when there are multiple files so
                // the rest of the scan can continue.
                if matches!(on_error, crate::cmd::copy::OnError::Fail) || files.len() == 1 {
                    return Err(msg.into());
                }
                warnings_out.push(msg);
            }
        }
    }

    // If every file in a multi-file search failed, returning Ok with
    // total_matches == 0 would silently look like a no-match result.
    // Return an error instead so callers know nothing was searched.
    if files_ok == 0 && !files.is_empty() {
        return Err(
            "find: no files could be searched; all entries were missing or unreadable".into(),
        );
    }

    // Total count line is only emitted when more than one file was matched.
    if count_only && multi_file {
        writeln!(out, "total: {total_matches}")?;
    }

    Ok(FindResult { total_matches })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_with_policy_warn_mode_all_missing_returns_error() {
        // When every supplied file is unreadable/missing in warn mode,
        // run_with_policy must error rather than silently returning Ok with
        // total_matches == 0 (which would look like a legitimate no-match).
        let mut out: Vec<u8> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let result = run_with_policy(
            &[
                "definitely_does_not_exist_a.txt",
                "definitely_does_not_exist_b.txt",
            ],
            &["pattern"],
            None,
            false,
            false,
            false,
            false,
            false,
            0,
            0,
            false,
            false,
            &mut out,
            IoMode::Buffered,
            crate::cmd::copy::OnError::Warn,
            &mut warnings,
        );
        assert!(
            result.is_err(),
            "expected error when all files are unreadable"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no files could be searched"),
            "error should mention 'no files could be searched', got: {msg}"
        );
        // Individual failures should have been recorded as warnings.
        assert_eq!(warnings.len(), 2);
    }

    // Helper: run search against in-memory bytes written to a temp file,
    // return the output as a String.
    #[allow(clippy::too_many_arguments)]
    fn search_bytes(
        content: &[u8],
        patterns: &[&str],
        fixed_string: bool,
        multiline: bool,
        all_match: bool,
        invert: bool,
        lines_before: usize,
        lines_after: usize,
        count_only: bool,
        numbers: bool,
    ) -> (String, usize) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), content).unwrap();
        let regexes = build_patterns(patterns, fixed_string, multiline, false).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        let count = run_single_file(
            tmp.path(),
            &regexes,
            all_match,
            invert,
            lines_before,
            lines_after,
            count_only,
            numbers,
            None,
            &mut buf,
            IoMode::Mmap,
        )
        .unwrap();
        (String::from_utf8(buf).unwrap(), count)
    }

    // Build a 10-line UTF-8/LF file: "line 1\nline 2\n...\nline 10\n"
    fn ten_line_file() -> Vec<u8> {
        (1..=10)
            .map(|i| format!("line {i}\n"))
            .collect::<String>()
            .into_bytes()
    }

    // ── Normal cases ──────────────────────────────────────────────────────────

    #[test]
    fn simple_match_finds_all_matching_lines() {
        let content = ten_line_file();
        let (out, count) = search_bytes(
            &content,
            &["line"],
            false,
            false,
            false,
            false,
            0,
            0,
            false,
            false,
        );
        assert_eq!(count, 10);
        assert_eq!(out.lines().count(), 10);
    }

    #[test]
    fn specific_pattern_finds_single_line() {
        let (out, count) = search_bytes(
            &ten_line_file(),
            &["line 5"],
            false,
            false,
            false,
            false,
            0,
            0,
            false,
            false,
        );
        assert_eq!(count, 1);
        assert!(out.contains("line 5"));
    }

    #[test]
    fn numbers_flag_prefixes_line_number() {
        let (out, _) = search_bytes(
            &ten_line_file(),
            &["line 7"],
            false,
            false,
            false,
            false,
            0,
            0,
            false,
            true,
        );
        assert!(out.starts_with("7:line 7"), "got: {out:?}");
    }

    #[test]
    fn count_mode_emits_number_only() {
        let (out, count) = search_bytes(
            &ten_line_file(),
            &["line"],
            false,
            false,
            false,
            false,
            0,
            0,
            true,
            false,
        );
        assert_eq!(count, 10);
        assert_eq!(out.trim(), "10");
    }

    #[test]
    fn invert_excludes_matching_lines() {
        let (out, count) = search_bytes(
            &ten_line_file(),
            &["line 5"],
            false,
            false,
            false,
            true,
            0,
            0,
            false,
            false,
        );
        assert_eq!(count, 9);
        assert!(!out.contains("line 5"));
    }

    #[test]
    fn fixed_strings_treats_metacharacters_as_literals() {
        // "line 5." with fixed_string should match "line 5." literally, not "line 50"
        let content = b"line 5.\nline 50\n";
        let (out, count) = search_bytes(
            content,
            &["line 5."],
            true,
            false,
            false,
            false,
            0,
            0,
            false,
            false,
        );
        assert_eq!(count, 1);
        assert!(out.contains("line 5."));
        assert!(!out.contains("line 50"));
    }

    #[test]
    fn no_match_returns_zero() {
        let (out, count) = search_bytes(
            &ten_line_file(),
            &["NOMATCH"],
            false,
            false,
            false,
            false,
            0,
            0,
            false,
            false,
        );
        assert_eq!(count, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn after_context_emits_lines_after_match() {
        let (out, _) = search_bytes(
            &ten_line_file(),
            &["line 3"],
            false,
            false,
            false,
            false,
            0,
            1,
            false,
            false,
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("line 3"));
        assert!(lines[1].contains("line 4"));
    }

    #[test]
    fn before_context_emits_lines_before_match() {
        let (out, _) = search_bytes(
            &ten_line_file(),
            &["line 3"],
            false,
            false,
            false,
            false,
            1,
            0,
            false,
            false,
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("line 2"));
        assert!(lines[1].contains("line 3"));
    }

    #[test]
    fn multiline_caret_dollar_at_lf_boundaries() {
        let content = b"hello\nworld\n";
        // With multiline, "^world$" should match the second line.
        let (_, count) = search_bytes(
            content,
            &["^world$"],
            false,
            true,
            false,
            false,
            0,
            0,
            false,
            false,
        );
        assert_eq!(count, 1);
    }

    #[test]
    fn or_mode_matches_either_pattern() {
        let (_, count) = search_bytes(
            &ten_line_file(),
            &["line 1$", "line 2$"],
            false,
            true,
            false,
            false,
            0,
            0,
            false,
            false,
        );
        // "line 1" (end-anchored) → should match "line 1" but not "line 10".
        // "line 2" → matches "line 2".
        // Total: 2 matches (line 1 and line 2).
        assert_eq!(count, 2);
    }

    #[test]
    fn and_mode_requires_all_patterns() {
        // "line" AND "5" must both match the same line.
        let (out, count) = search_bytes(
            &ten_line_file(),
            &["line", "5"],
            false,
            false,
            true,
            false,
            0,
            0,
            false,
            false,
        );
        assert_eq!(count, 1);
        assert!(out.contains("line 5"));
    }

    // ── Edge cases ────────────────────────────────────────────────────────────

    #[test]
    fn empty_file_returns_zero_matches() {
        let (out, count) = search_bytes(
            b"",
            &["anything"],
            false,
            false,
            false,
            false,
            0,
            0,
            false,
            false,
        );
        assert_eq!(count, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn empty_file_count_mode_emits_zero() {
        let (out, count) = search_bytes(b"", &["x"], false, false, false, false, 0, 0, true, false);
        assert_eq!(count, 0);
        assert_eq!(out.trim(), "0");
    }

    #[test]
    fn count_invert_nomatch_counts_all_lines() {
        // --count --invert --pattern "NOMATCH" → all lines match the inverted predicate
        let (out, count) = search_bytes(
            &ten_line_file(),
            &["NOMATCH"],
            false,
            false,
            false,
            true,
            0,
            0,
            true,
            false,
        );
        assert_eq!(count, 10);
        assert_eq!(out.trim(), "10");
    }

    #[test]
    fn context_deduplication_no_double_emit() {
        // Match on lines 3 and 5 with -A 2 / -B 2; line 4 is both after-context
        // of 3 and before-context of 5 — it must appear only once.
        let content = b"a\nb\nc\nd\ne\nf\n";
        let (out, _) = search_bytes(
            content,
            &["^c$|^e$"],
            false,
            true,
            false,
            false,
            1,
            1,
            false,
            true,
        );
        // Expected emitted lines: b(ctx), c(match), d(ctx), e(match), f(ctx)
        let lines: Vec<&str> = out.lines().collect();
        // --- line "d" must appear only once
        let d_count = lines.iter().filter(|l| l.contains("d")).count();
        assert_eq!(
            d_count, 1,
            "line 'd' should appear exactly once; output:\n{out}"
        );
    }

    #[test]
    fn separator_emitted_between_non_adjacent_groups() {
        let content = ten_line_file();
        // Match lines 2 and 8 with -A 1; there should be a "--" separator between groups.
        let (out, _) = search_bytes(
            &content,
            &["line 2$|line 8$"],
            false,
            true,
            false,
            false,
            0,
            1,
            false,
            false,
        );
        assert!(
            out.contains("--"),
            "expected '--' separator; output:\n{out}"
        );
    }

    #[test]
    fn invert_and_all_match_emits_lines_failing_at_least_one_pattern() {
        // --invert --all-match: emit lines that fail at least one pattern.
        // Pattern "line" matches everything; pattern "5" matches only "line 5".
        // With all_match+invert: lines where NOT all match → 9 lines (all except "line 5").
        let (_, count) = search_bytes(
            &ten_line_file(),
            &["line", "5"],
            false,
            false,
            true,
            true,
            0,
            0,
            false,
            false,
        );
        assert_eq!(count, 9);
    }

    #[test]
    fn single_line_file_no_terminator() {
        // A file with a single line and no trailing newline.
        let (out, count) = search_bytes(
            b"hello",
            &["hello"],
            false,
            false,
            false,
            false,
            0,
            0,
            false,
            false,
        );
        assert_eq!(count, 1);
        assert_eq!(out.trim(), "hello");
    }

    #[test]
    fn match_on_line_1_before_context_is_empty() {
        // With -B 2, a match on line 1 has no before-context.
        let (out, _) = search_bytes(
            &ten_line_file(),
            &["line 1$"],
            false,
            true,
            false,
            false,
            2,
            0,
            false,
            true,
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("line 1"));
    }

    #[test]
    fn match_on_last_line_after_context_is_empty() {
        let (out, _) = search_bytes(
            &ten_line_file(),
            &["line 10"],
            false,
            false,
            false,
            false,
            0,
            2,
            false,
            true,
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("line 10"));
    }

    #[test]
    fn line_match_uses_colon_context_uses_dash() {
        let (out, _) = search_bytes(
            &ten_line_file(),
            &["line 5"],
            false,
            false,
            false,
            false,
            1,
            1,
            false,
            true,
        );
        let lines: Vec<&str> = out.lines().collect();
        // Line 4: context (dash), line 5: match (colon), line 6: context (dash)
        assert!(
            lines.iter().any(|l| l.starts_with("4-")),
            "before-context should use '-'; output:\n{out}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("5:")),
            "match should use ':'; output:\n{out}"
        );
        assert!(
            lines.iter().any(|l| l.starts_with("6-")),
            "after-context should use '-'; output:\n{out}"
        );
    }

    // ── expand_paths: directory-spec error behavior ───────────────────────────

    #[test]
    fn expand_paths_dot_slash_suggests_star_star() {
        // "./" normalises to "" after stripping the "./" prefix; the suggested
        // glob should be "**" (not the broken "/**").
        let err = expand_paths(&["./"]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("is a directory"), "got: {msg}");
        assert!(
            msg.contains("\"**\""),
            "expected '\"**\"' in suggestion; got: {msg}"
        );
        assert!(
            !msg.contains("\"/**\""),
            "must not suggest '\"/**\"'; got: {msg}"
        );
    }

    #[test]
    fn expand_paths_dot_alone_suggests_star_star() {
        // "." is the walker root; same rule as "./" applies.
        let err = expand_paths(&["."]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("is a directory"), "got: {msg}");
        assert!(
            msg.contains("\"**\""),
            "expected '\"**\"' in suggestion; got: {msg}"
        );
    }

    #[test]
    fn expand_paths_relative_dir_suggests_subdir_glob() {
        // Create a temporary subdirectory inside the current directory so we
        // can pass a relative name to expand_paths.
        let dir = tempfile::Builder::new()
            .prefix("tpu_test_expand_")
            .tempdir_in(".")
            .unwrap();
        let dirname = dir.path().file_name().unwrap().to_str().unwrap().to_owned();
        let err = expand_paths(&[dirname.as_str()]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("is a directory"), "got: {msg}");
        // The suggestion should end with "/**" and not start with "/"
        assert!(
            msg.contains(&format!("\"{dirname}/**\"")),
            "expected '{dirname}/**' in suggestion; got: {msg}"
        );
    }

    #[test]
    fn expand_paths_absolute_dir_suggests_glob_param() {
        let dir = tempfile::TempDir::new().unwrap();
        let abs = dir.path().to_str().unwrap();
        let err = expand_paths(&[abs]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("is a directory"), "got: {msg}");
        // The new guidance points the caller at the `glob` parameter,
        // which walks the supplied directory directly.
        assert!(
            msg.contains("`glob`"),
            "should mention the `glob` parameter; got: {msg}"
        );
    }

    // ── expand_paths_with_policy: --glob mode ─────────────────────────────────

    /// Helper: create a temp directory populated with the given relative
    /// file paths.  Each file is created empty.
    fn make_dir_with_files(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        for rel in files {
            let p = dir.path().join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, b"").unwrap();
        }
        dir
    }

    #[test]
    fn expand_paths_glob_walks_absolute_directory() {
        // The motivating bug: an absolute directory + glob filter should
        // walk that directory and return only files matching the glob.
        let dir = make_dir_with_files(&[
            "md31_t1.ndjson",
            "md31_t2.ndjson",
            "other.ndjson",
            "subdir/md31_t3.ndjson",
            "subdir/notes.txt",
        ]);
        let abs = dir.path().to_str().unwrap().to_owned();
        let mut warnings: Vec<String> = Vec::new();
        let paths = expand_paths_with_policy(
            &[abs.as_str()],
            Some("**/md31_t*.ndjson"),
            crate::cmd::copy::OnError::Warn,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(paths.len(), 3, "got: {paths:?}");
        for p in &paths {
            let name = p.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with("md31_t"), "unexpected: {name}");
        }
    }

    #[test]
    fn expand_paths_glob_matches_no_files_errors() {
        let dir = make_dir_with_files(&["a.txt", "b.txt"]);
        let abs = dir.path().to_str().unwrap().to_owned();
        let mut warnings: Vec<String> = Vec::new();
        let err = expand_paths_with_policy(
            &[abs.as_str()],
            Some("**/*.ndjson"),
            crate::cmd::copy::OnError::Warn,
            &mut warnings,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("matched no files"), "got: {msg}");
    }

    #[test]
    fn expand_paths_glob_passes_through_file_specs() {
        // A literal file path is included as-is when `glob` is supplied;
        // the glob does not filter explicitly-named files.
        let dir = make_dir_with_files(&["only.txt"]);
        let file = dir.path().join("only.txt");
        let file_str = file.to_str().unwrap().to_owned();
        let mut warnings: Vec<String> = Vec::new();
        let paths = expand_paths_with_policy(
            &[file_str.as_str()],
            Some("**/*.ndjson"),
            crate::cmd::copy::OnError::Warn,
            &mut warnings,
        )
        .unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], file);
    }

    #[test]
    fn expand_paths_glob_rejects_glob_in_path_spec() {
        // Mixing a glob-shaped path spec with a separate `glob` is
        // ambiguous and must be rejected.
        let mut warnings: Vec<String> = Vec::new();
        let err = expand_paths_with_policy(
            &["some/**/path"],
            Some("*.txt"),
            crate::cmd::copy::OnError::Warn,
            &mut warnings,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("glob metacharacters") && msg.contains("pick one form"),
            "got: {msg}"
        );
    }
}
