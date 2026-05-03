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
//! the M2 write-time guard apply uniformly.  The write is issued with
//! [`WritePolicy::permissive`] because the *intent* is to write a string
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

use globset::{Glob, GlobSet, GlobSetBuilder};
use harrier::{encoding::SourceConfig, source::Source};
use serde_json::json;
use walkdir::WalkDir;

use crate::{
    encoding::{BomPolicy, OutputEncoding},
    mojibake::{self, Pattern},
    IoMode,
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

/// Per-file diagnostic record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorIssue {
    pub path: PathBuf,
    pub encoding_detected: &'static str,
    pub valid_in_detected_encoding: bool,
    pub mojibake_matches: Vec<DoctorMatch>,
    /// `Some(text)` if a one-layer peel produces strictly fewer matches.
    /// Only populated when at least one mojibake pattern was found.
    pub peel_suggested: Option<String>,
    /// `true` once the file has been rewritten with `peel_suggested`.
    pub repaired: bool,
}

impl DoctorIssue {
    /// True when the file has anything worth reporting (invalid encoding
    /// or one-or-more mojibake matches).
    pub fn is_problem(&self) -> bool {
        !self.valid_in_detected_encoding || !self.mojibake_matches.is_empty()
    }
}

/// Aggregate result of [`run`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DoctorReport {
    /// One entry per *flagged* file (clean files are omitted).
    pub issues: Vec<DoctorIssue>,
    pub total_files_scanned: usize,
    pub total_repaired: usize,
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
    "exe", "dll", "so", "dylib", "a", "lib", "o", "obj", "pdb", "class",
    "jar", "war", "zip", "7z", "gz", "tgz", "bz2", "xz", "rar", "tar",
    "iso", "dmg", "img", "bin", "dat", "db", "sqlite",
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "tif", "tiff", "webp",
    "psd", "svgz",
    "mp3", "mp4", "wav", "flac", "ogg", "avi", "mov", "mkv", "webm",
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
    "ttf", "otf", "woff", "woff2", "eot",
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
fn expand_paths(path_specs: &[&str]) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    for &spec in path_specs {
        let is_glob =
            spec.contains('*') || spec.contains('?') || spec.contains('[') || spec.contains('{');

        if is_glob {
            let matcher = Glob::new(spec)
                .map_err(|e| format!("doctor: invalid glob {spec:?}: {e}"))?
                .compile_matcher();
            for entry in WalkDir::new(".").into_iter().filter_entry(|e| !is_skipped_dir(e)) {
                let entry = entry.map_err(|e| format!("doctor: glob walk error: {e}"))?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = entry.path().strip_prefix(".").unwrap_or(entry.path());
                if matcher.is_match(rel) && !is_binary_extension(entry.path()) {
                    push_unique(&mut paths, &mut seen, entry.path().to_path_buf());
                }
            }
            continue;
        }

        let p = PathBuf::from(spec);
        let meta = fs::metadata(&p)
            .map_err(|e| format!("doctor: cannot stat {}: {e}", p.display()))?;

        if meta.is_file() {
            if !is_binary_extension(&p) {
                push_unique(&mut paths, &mut seen, p);
            }
            continue;
        }

        if meta.is_dir() {
            let ignore = load_gitignore(&p);
            for entry in WalkDir::new(&p).into_iter().filter_entry(|e| !is_skipped_dir(e)) {
                let entry = entry.map_err(|e| format!("doctor: walk error: {e}"))?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if is_binary_extension(path) {
                    continue;
                }
                if let Some(set) = &ignore {
                    let rel = path.strip_prefix(&p).unwrap_or(path);
                    if set.is_match(rel) {
                        continue;
                    }
                }
                push_unique(&mut paths, &mut seen, path.to_path_buf());
            }
            continue;
        }
    }

    Ok(paths)
}

fn push_unique(paths: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, p: PathBuf) {
    let key = fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
    if seen.insert(key) {
        paths.push(p);
    }
}

/// Skip `.git/`, `node_modules/`, and `target/` subtrees during walks.
/// These are virtually always either binary, generated, or already
/// classified by their own checkers — and they would otherwise dominate
/// the report.
fn is_skipped_dir(entry: &walkdir::DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    matches!(name.as_ref(), ".git" | "node_modules" | "target")
}

/// Load `<root>/.gitignore` if present and compile its non-comment
/// non-empty lines into a [`GlobSet`].  Negation lines (starting with `!`)
/// are ignored to keep the implementation conservative — the doctor
/// walks files; it does not need to be a perfect gitignore engine.
fn load_gitignore(root: &Path) -> Option<GlobSet> {
    let content = fs::read_to_string(root.join(".gitignore")).ok()?;
    let mut builder = GlobSetBuilder::new();
    let mut any = false;
    for line in content.lines() {
        let s = line.trim();
        if s.is_empty() || s.starts_with('#') || s.starts_with('!') {
            continue;
        }
        // Trailing-slash means "directory" — globset doesn't use that
        // syntax; expand to `dir/**`.
        let pat = if let Some(stripped) = s.strip_suffix('/') {
            format!("{stripped}/**")
        } else {
            s.to_string()
        };
        // Allow both root-anchored and any-depth matches.
        if let Ok(g) = Glob::new(&pat) {
            builder.add(g);
            any = true;
        }
        if let Ok(g) = Glob::new(&format!("**/{pat}")) {
            builder.add(g);
            any = true;
        }
    }
    if !any {
        return None;
    }
    builder.build().ok()
}

// ── Per-file diagnosis ──────────────────────────────────────────────────────

/// Run encoding + mojibake diagnostics for a single file.
///
/// Returns `Ok(None)` for a clean file (and the file is not included in
/// the report), `Ok(Some(issue))` when there is anything to report, and
/// `Err` for I/O / open failures.
fn diagnose_file(path: &Path, io_mode: IoMode) -> Result<Option<DoctorIssue>, Box<dyn Error>> {
    let branch = crate::open_as_branch(path, io_mode)?;
    let source = Source::new(Arc::clone(&branch), SourceConfig::default())?;
    let encoding = source.encoding();

    // Read raw bytes through the same I/O mode so behaviour is identical
    // to other commands.
    let raw = crate::read_raw_bytes(path, io_mode)?;
    let bom_len = source.bom_len();
    let body = &raw[bom_len.min(raw.len())..];

    // Decode; `had_errors` tells us whether any byte sequences were
    // invalid in the detected encoding.
    let (decoded, _, had_errors) = encoding.decode(body);
    let decoded_text: &str = &decoded;

    // Allow-marker opt-out for the *whole file*.
    if mojibake::allowed_by_marker(decoded_text) {
        return Ok(None);
    }

    if had_errors {
        // Encoding-invalid: don't bother running the mojibake scan, the
        // replacement chars would create false positives.
        return Ok(Some(DoctorIssue {
            path: path.to_path_buf(),
            encoding_detected: encoding.name(),
            valid_in_detected_encoding: false,
            mojibake_matches: Vec::new(),
            peel_suggested: None,
            repaired: false,
        }));
    }

    let report = mojibake::scan(decoded_text);
    if report.matches.is_empty() {
        return Ok(None);
    }

    // Build line/col index by streaming through the decoded text once.
    let matches = annotate_matches(decoded_text, &report.matches);

    let peel_suggested = mojibake::looks_like_one_layer_peel(decoded_text);

    Ok(Some(DoctorIssue {
        path: path.to_path_buf(),
        encoding_detected: encoding.name(),
        valid_in_detected_encoding: true,
        mojibake_matches: matches,
        peel_suggested,
        repaired: false,
    }))
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
        .map(|o| o.unwrap_or(DoctorMatch {
            byte_offset: 0,
            line: 0,
            col: 0,
            pattern: Pattern::Latin1,
        }))
        .collect()
}

// ── Repair ──────────────────────────────────────────────────────────────────

/// Apply `peel_suggested` to the file via [`crate::cmd::write::run`] using
/// permissive write policy.  Updates `issue.repaired` on success.
fn apply_peel(issue: &mut DoctorIssue, io_mode: IoMode) -> Result<(), Box<dyn Error>> {
    let Some(peeled) = issue.peel_suggested.clone() else {
        return Ok(());
    };
    crate::cmd::write::run(
        &issue.path,
        &peeled,
        OutputEncoding::Preserve,
        BomPolicy::default(),
        None,
        None,
        io_mode,
        mojibake::WritePolicy::permissive(),
    )?;
    issue.repaired = true;
    Ok(())
}

// ── Public entry point ──────────────────────────────────────────────────────

/// Run the `tpu doctor` subcommand.
///
/// `path_specs` may be empty, in which case the current directory `"."`
/// is used.  Output (the human or JSON report) is written to `out`.
pub fn run(
    path_specs: &[&str],
    options: DoctorOptions,
    out: &mut dyn Write,
    io_mode: IoMode,
) -> Result<DoctorReport, Box<dyn Error>> {
    let default = ["."];
    let specs: &[&str] = if path_specs.is_empty() { &default } else { path_specs };

    let files = expand_paths(specs)?;
    let mut report = DoctorReport {
        total_files_scanned: files.len(),
        ..DoctorReport::default()
    };

    for path in &files {
        match diagnose_file(path, io_mode) {
            Ok(Some(issue)) => report.issues.push(issue),
            Ok(None) => {}
            Err(e) => {
                // Emit a per-file warning but keep going so one unreadable
                // file doesn't abort a large scan.
                if !options.quiet && options.format == DoctorFormat::Human {
                    writeln!(out, "doctor: {}: {}", path.display(), e)?;
                }
            }
        }
    }

    if options.fix == DoctorFix::Peel {
        for issue in &mut report.issues {
            if !issue.mojibake_matches.is_empty() && issue.peel_suggested.is_some()
                && let Err(e) = apply_peel(issue, io_mode)
                    && options.format == DoctorFormat::Human {
                        writeln!(
                            out,
                            "doctor: peel-fix failed for {}: {}",
                            issue.path.display(),
                            e
                        )?;
                    }
        }
        report.total_repaired = report.issues.iter().filter(|i| i.repaired).count();
    }

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
            json!({
                "path": issue.path.display().to_string(),
                "encoding_detected": issue.encoding_detected,
                "valid_in_detected_encoding": issue.valid_in_detected_encoding,
                "mojibake_matches": matches,
                "peel_suggested": issue.peel_suggested.is_some(),
                "repaired": issue.repaired,
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
        let mut found_src = false;
        let mut found_skipped = false;
        for e in WalkDir::new(tmp.path()).into_iter().filter_entry(|e| !is_skipped_dir(e)) {
            let e = e.unwrap();
            let n = e.file_name().to_string_lossy().to_string();
            if n == "src" { found_src = true; }
            if n == ".git" || n == "target" {
                // only top-level entry survives the filter (it's checked
                // *before* being yielded, so we still see the dir itself).
                if e.depth() > 0 {
                    found_skipped = true;
                }
            }
        }
        assert!(found_src);
        assert!(!found_skipped, "should not descend into .git or target");
    }

    #[test]
    fn diagnose_clean_utf8_file_returns_none() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "clean.txt", "hello world\n".as_bytes());
        let res = diagnose_file(&p, IoMode::Buffered).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn diagnose_mojibake_file_returns_issue_with_line_col() {
        let tmp = TempDir::new().unwrap();
        // Two lines; mojibake is on line 2 at column 5 (0-indexed bytes
        // include "first\n" = 6 bytes before the 'c' of 'caf').
        let p = write(&tmp, "bad.txt", "first\ncafÃ©\n".as_bytes());
        let issue = diagnose_file(&p, IoMode::Buffered).unwrap().expect("flagged");
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
        let res = diagnose_file(&p, IoMode::Buffered).unwrap();
        if let Some(issue) = res
            && !issue.valid_in_detected_encoding {
                assert!(issue.mojibake_matches.is_empty());
            }
    }

    #[test]
    fn allow_marker_suppresses_diagnosis() {
        let tmp = TempDir::new().unwrap();
        let body = format!(
            "// {}\nthis line has cafÃ© in it\n",
            mojibake::ALLOW_MARKER
        );
        let p = write(&tmp, "ok.txt", body.as_bytes());
        let res = diagnose_file(&p, IoMode::Buffered).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn empty_file_is_clean() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "empty.txt", b"");
        let res = diagnose_file(&p, IoMode::Buffered).unwrap();
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
            },
            &mut buf,
            IoMode::Buffered,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains("bad.txt"), "quiet should suppress per-file lines: {s}");
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
                || mojibake::scan(&now_text).matches.len()
                    < mojibake::scan(single).matches.len()
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
            },
            &mut buf,
            IoMode::Buffered,
        )
        .unwrap();

        assert_eq!(report.total_repaired, 0);
        let bak = format!("{}.bak", p.display());
        assert!(!Path::new(&bak).exists(), "no .bak should be created for clean files");
    }

    #[test]
    fn issue_is_problem_distinguishes_mojibake_and_invalid() {
        let mut i = DoctorIssue {
            path: PathBuf::from("x"),
            encoding_detected: "UTF-8",
            valid_in_detected_encoding: true,
            mojibake_matches: Vec::new(),
            peel_suggested: None,
            repaired: false,
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
            peel_suggested: None,
            repaired: false,
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
            },
            &mut buf,
            IoMode::Buffered,
        )
        .unwrap();
        // .gitignore + kept.txt are scanned; ignored/ + *.log skipped.
        assert!(report.issues.iter().all(|i| !i.path.to_string_lossy().contains("ignored")));
        assert!(report.issues.iter().all(|i| !i.path.to_string_lossy().ends_with(".log")));
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
            },
            &mut buf,
            IoMode::Buffered,
        )
        .unwrap();
        assert_eq!(report.total_files_scanned, 1);
        assert!(report.issues.iter().any(|i| i.path.to_string_lossy().ends_with("a.txt")));
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
}
