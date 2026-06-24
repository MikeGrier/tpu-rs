// Copyright (c) 2026, Michael Grier

//! Git-aware line-ending detection using gitoxide (pure Rust, no `git` binary).
//!
//! This module answers a single question: *does a file's on-disk line-ending
//! convention match what git would materialise in the working tree* for that
//! path, given the repository's `.gitattributes` rules and the `core.autocrlf`
//! / `core.eol` configuration?
//!
//! It is **opt-in per call**: nothing here runs unless a caller explicitly
//! supplies a repository root.  There is no upward auto-discovery and no
//! ambient/global enablement — the caller is always in control.
//!
//! The expected working-tree line ending is computed exactly the way git (and
//! [`gix_filter`]) computes it: the `text`, `crlf` and `eol` attributes are
//! combined into an [`AttributesDigest`], folded together with the repository's
//! [`Configuration`] (`core.autocrlf` / `core.eol`), and reduced to a target
//! [`Mode`] (LF or CRLF) — or to "no normalisation" for binary / `-text`
//! paths.  We then compare that expectation against the bytes actually on disk.

use std::path::{Path, PathBuf};

use gix::filter::plumbing::eol::{AttributesDigest, Configuration, Mode, Stats};
use harrier::encoding::LineEnding;

/// A detected disagreement between a file's on-disk line endings and the
/// convention git expects for that path in the working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EolMismatch {
    /// The line ending git would materialise in the working tree (the
    /// normalisation target, suitable for a `--fix` rewrite).
    pub expected: LineEnding,
    /// The dominant line ending actually present in the file on disk.
    pub actual: LineEnding,
}

/// Convenience boxed-error alias matching the rest of the crate.
type BoxError = Box<dyn std::error::Error>;

/// An opened repository handle used to resolve the expected working-tree line
/// ending for one or more paths.
///
/// Construct with [`GitEol::open`].  A single handle can resolve many files in
/// the same repository (used by `tpu doctor`); single-file callers can use the
/// free [`detect`] helper instead.
pub struct GitEol {
    repo: gix::Repository,
    index: gix::worktree::Index,
    config: Configuration,
    workdir: PathBuf,
}

impl GitEol {
    /// Open the repository located **exactly** at `root` (no upward
    /// discovery).  Returns `Ok(None)` if `root` has no working tree (e.g. a
    /// bare repository), in which case there is nothing to normalise.
    pub fn open(root: &Path) -> Result<Option<Self>, BoxError> {
        let repo = gix::open(root)?;
        let Some(workdir) = repo.workdir().map(Path::to_path_buf) else {
            return Ok(None);
        };
        // Canonicalise the working-tree root once here so per-file path
        // resolution (`repo_relative`) doesn't pay for it on every lookup.
        let workdir = std::fs::canonicalize(&workdir).unwrap_or(workdir);
        // The eol Configuration (core.autocrlf / core.eol) is most reliably
        // obtained by asking gix's own filter pipeline for its options.
        let config = gix::filter::Pipeline::options(&repo)?.eol_config;
        let index = repo.index_or_empty()?;
        Ok(Some(GitEol {
            repo,
            index,
            config,
            workdir,
        }))
    }

    /// Detect whether `bytes` (the current on-disk contents of `file`) disagree
    /// with git's expected working-tree line ending for that path.
    ///
    /// Returns `Ok(None)` when the path is outside the working tree, when git
    /// would not normalise it (binary / `-text` / no applicable rule), or when
    /// the file already matches the expectation.
    pub fn detect(&self, file: &Path, bytes: &[u8]) -> Result<Option<EolMismatch>, BoxError> {
        let Some(digest) = self.digest_for(file)? else {
            return Ok(None);
        };
        Ok(detect_with_digest(bytes, digest, self.config))
    }

    /// The git line ending git expects in the working tree for `file`, or
    /// `None` if git would not normalise this path.
    pub fn expected_line_ending(&self, file: &Path) -> Result<Option<LineEnding>, BoxError> {
        let Some(digest) = self.digest_for(file)? else {
            return Ok(None);
        };
        Ok(digest.to_eol(self.config).map(mode_to_line_ending))
    }

    /// Build the read-time advisory line for `file` using this already-open
    /// handle, or `None` when there is no mismatch.
    ///
    /// Only the first [`ADVISORY_SCAN_CAP`] bytes are read: an advisory is
    /// best-effort and bounding the read keeps `tpu head`/`tail` (and reads of
    /// very large files) cheap instead of forcing a whole-file read.  Bounding
    /// can only *miss* a mismatch that first appears past the cap; it never
    /// produces a spurious one.  This also mirrors git, which sniffs only the
    /// first several kilobytes when classifying content.
    pub fn advisory_note(&self, file: &Path) -> Option<String> {
        let bytes = read_capped(file, ADVISORY_SCAN_CAP).ok()?;
        let mismatch = self.detect(file, &bytes).ok().flatten()?;
        Some(format!(
            "note: {}: line endings ({}) differ from git's expected {} (per .gitattributes / core.autocrlf / core.eol); run 'tpu doctor' to normalize",
            file.display(),
            line_ending_name(mismatch.actual),
            line_ending_name(mismatch.expected),
        ))
    }

    /// Resolve the folded [`AttributesDigest`] for `file`, mirroring
    /// `gix_filter`'s pipeline configuration logic.
    fn digest_for(&self, file: &Path) -> Result<Option<AttributesDigest>, BoxError> {
        let Some(rela) = self.repo_relative(file) else {
            return Ok(None);
        };

        // Attributes are read from on-disk `.gitattributes` (worktree) with the
        // index as a fallback for in-tree files.
        let mut stack = self.repo.attributes_only(
            &self.index,
            gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
        )?;
        let platform = stack.at_path(&rela, None)?;

        let mut outcome = gix::attrs::search::Outcome::default();
        // Selection order is significant: index 0 = crlf, 1 = eol, 2 = text.
        outcome.initialize_with_selection(&Default::default(), ["crlf", "eol", "text"]);
        platform.matching_attributes(&mut outcome);

        let selected: Vec<_> = outcome.iter_selected().collect();
        let crlf_attr = selected[0].assignment.state;
        let eol_attr = selected[1].assignment.state;
        let text_attr = selected[2].assignment.state;

        Ok(Some(fold_digest(
            text_attr,
            crlf_attr,
            eol_attr,
            self.config,
        )))
    }

    /// Map an absolute or relative path to a path relative to the working-tree
    /// root, or `None` if it lies outside the working tree.
    fn repo_relative(&self, file: &Path) -> Option<PathBuf> {
        let file_abs = canonicalize_lenient(file)?;
        file_abs
            .strip_prefix(&self.workdir)
            .ok()
            .map(Path::to_path_buf)
    }
}

/// Detect a line-ending mismatch for a single file by opening the repository at
/// `root`.  Convenience wrapper over [`GitEol`] for single-file callers (reads).
///
/// Any error opening the repository or resolving attributes is returned to the
/// caller, which may choose to treat it as "no advisory".
pub fn detect(root: &Path, file: &Path, bytes: &[u8]) -> Result<Option<EolMismatch>, BoxError> {
    match GitEol::open(root)? {
        Some(git) => git.detect(file, bytes),
        None => Ok(None),
    }
}

/// Upper bound on bytes read for the best-effort read-time EOL advisory.
/// Files are overwhelmingly smaller than this; bounding the scan keeps
/// `head`/`tail` and large-file reads cheap (see [`GitEol::advisory_note`]).
const ADVISORY_SCAN_CAP: u64 = 1 << 20; // 1 MiB

/// Read up to `cap` bytes from `file` without ever loading more than that into
/// memory (used by the bounded read-time advisory).
fn read_capped(file: &Path, cap: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(file)?.take(cap).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Resolve the line-ending override for a mutating write
/// (`write`/`replace`/`edit`/`append`), shared by the `tpu` CLI and `tpu-mcp`.
///
/// An explicit token (`"lf"`/`"crlf"`/`"cr"`) always wins and is parsed via the
/// canonical [`crate::encoding::parse_line_ending`].  Otherwise, when git-EOL
/// normalisation is enabled (`eol_normalize`) and a `git_root` is supplied, the
/// override is git's expected convention for `file`.  Returns `Ok(None)` when
/// neither applies, in which case the file's own dominant ending is preserved.
pub fn resolve_write_override(
    explicit: Option<&str>,
    file: &Path,
    git_root: Option<&Path>,
    eol_normalize: bool,
) -> Result<Option<LineEnding>, BoxError> {
    if let Some(s) = explicit {
        return Ok(Some(crate::encoding::parse_line_ending(s)?));
    }
    if eol_normalize
        && let Some(root) = git_root
        && let Ok(Some(g)) = GitEol::open(root)
        && let Ok(expected) = g.expected_line_ending(file)
    {
        return Ok(expected);
    }
    Ok(None)
}

/// Canonicalise `file`, tolerating a not-yet-existing leaf (canonicalise the
/// parent and re-attach the file name) so write-time callers can resolve the
/// expectation for a path before it exists on disk.
fn canonicalize_lenient(file: &Path) -> Option<PathBuf> {
    if let Ok(c) = std::fs::canonicalize(file) {
        return Some(c);
    }
    let parent = file.parent().filter(|p| !p.as_os_str().is_empty())?;
    let name = file.file_name()?;
    let parent = std::fs::canonicalize(parent).ok()?;
    Some(parent.join(name))
}

/// Combine the `text`, `crlf` and `eol` attribute states with the repository
/// configuration into a final [`AttributesDigest`].
///
/// This is a faithful re-implementation of `gix_filter`'s pipeline
/// configuration logic (`Configuration::at_path`), restricted to the three
/// attributes that affect end-of-line handling.
///
/// # Pinned to gitoxide internals
///
/// This mirrors private logic in `gix-filter` (as of **gix 0.85**); that
/// algorithm is not a stability guarantee, so a gix bump could silently change
/// what git considers "expected".  The integration-style unit tests below init
/// real repositories and assert the end-to-end expectation, so any divergence
/// after an upgrade surfaces as a test failure rather than a silent wrong
/// answer.  When bumping gix, re-verify against `gix-filter`'s
/// `Configuration::at_path` / `eol` module.
fn fold_digest(
    text: gix::attrs::StateRef,
    crlf: gix::attrs::StateRef,
    eol: gix::attrs::StateRef,
    config: Configuration,
) -> AttributesDigest {
    let mut digest = extract_crlf(text);
    if digest.is_none() {
        digest = extract_crlf(crlf);
    }

    if digest != Some(AttributesDigest::Binary) {
        let eol_mode = extract_eol(eol);
        digest = match digest {
            Some(AttributesDigest::TextAuto) if eol_mode == Some(Mode::Lf) => {
                Some(AttributesDigest::TextAutoInput)
            }
            Some(AttributesDigest::TextAuto) if eol_mode == Some(Mode::CrLf) => {
                Some(AttributesDigest::TextAutoCrlf)
            }
            _ => match eol_mode {
                Some(Mode::CrLf) => Some(AttributesDigest::TextCrlf),
                Some(Mode::Lf) => Some(AttributesDigest::TextInput),
                None => digest,
            },
        };
    }

    match digest {
        None => AttributesDigest::from(config.auto_crlf),
        Some(AttributesDigest::Text) => AttributesDigest::from(config.to_eol()),
        Some(other) => other,
    }
}

/// Map the `text` / `crlf` attribute state to a digest (git's
/// `git_path_check_crlf`).
fn extract_crlf(state: gix::attrs::StateRef) -> Option<AttributesDigest> {
    use gix::attrs::StateRef;
    match state {
        StateRef::Unspecified => None,
        StateRef::Set => Some(AttributesDigest::Text),
        StateRef::Unset => Some(AttributesDigest::Binary),
        StateRef::Value(v) => {
            let v = v.as_bstr();
            if v == "input" {
                Some(AttributesDigest::TextInput)
            } else if v == "auto" {
                Some(AttributesDigest::TextAuto)
            } else {
                None
            }
        }
    }
}

/// Map the `eol` attribute state to an explicit [`Mode`].
fn extract_eol(state: gix::attrs::StateRef) -> Option<Mode> {
    use gix::attrs::StateRef;
    match state {
        StateRef::Unspecified | StateRef::Unset | StateRef::Set => None,
        StateRef::Value(v) => {
            let v = v.as_bstr();
            if v == "lf" {
                Some(Mode::Lf)
            } else if v == "crlf" {
                Some(Mode::CrLf)
            } else {
                None
            }
        }
    }
}

/// Compare a buffer's actual line endings against the expectation implied by
/// `digest` + `config`.
fn detect_with_digest(
    bytes: &[u8],
    digest: AttributesDigest,
    config: Configuration,
) -> Option<EolMismatch> {
    // `to_eol` returns `None` when git would not normalise (binary / -text).
    let expected = digest.to_eol(config)?;
    let stats = Stats::from_bytes(bytes);

    // In `auto` modes git leaves binary content untouched, so a binary buffer
    // is never a mismatch even though an EOL would otherwise be expected.
    if digest.is_auto_text() && stats.is_binary() {
        return None;
    }

    // A mismatch exists only when the buffer contains a line ending git would
    // *not* materialise, and `actual` names the dominant *non-conforming*
    // ending rather than the overall dominant ending.  This keeps the report
    // honest for mixed-ending files — a mostly-CRLF file with a few bare LFs
    // is still flagged, and `actual` reports the offending `LF` instead of
    // `CRLF` (which would equal `expected` and read as a contradiction).  It
    // also means lone `CR`s are treated as non-conforming when git expects
    // either `CRLF` or `LF`.
    let actual = match expected {
        // git wants CRLF: bare LFs and lone CRs are both wrong.
        Mode::CrLf => dominant_of(&[
            (LineEnding::Lf, stats.lone_lf as u64),
            (LineEnding::Cr, stats.lone_cr as u64),
        ]),
        // git wants LF: CRLFs and lone CRs are both wrong.
        Mode::Lf => dominant_of(&[
            (LineEnding::CrLf, stats.crlf as u64),
            (LineEnding::Cr, stats.lone_cr as u64),
        ]),
    }?;

    Some(EolMismatch {
        expected: mode_to_line_ending(expected),
        actual,
    })
}

/// The candidate line ending with the largest non-zero count, or `None` when
/// every candidate count is zero.  Used to pick the dominant *non-conforming*
/// ending so a reported `actual` is always meaningful.
fn dominant_of(candidates: &[(LineEnding, u64)]) -> Option<LineEnding> {
    candidates
        .iter()
        .copied()
        .filter(|(_, n)| *n > 0)
        .max_by_key(|(_, n)| *n)
        .map(|(le, _)| le)
}

/// Translate a git [`Mode`] into the crate's [`LineEnding`] vocabulary.
fn mode_to_line_ending(mode: Mode) -> LineEnding {
    match mode {
        Mode::Lf => LineEnding::Lf,
        Mode::CrLf => LineEnding::CrLf,
    }
}

/// Short human-readable name for a line ending (`LF`, `CRLF`, `CR`).
pub fn line_ending_name(le: LineEnding) -> &'static str {
    match le {
        LineEnding::Lf => "LF",
        LineEnding::CrLf => "CRLF",
        LineEnding::Cr => "CR",
    }
}

/// Read-time advisory for git line-ending mismatches.
///
/// When `git_root` points at a repository and `file`'s on-disk line endings
/// disagree with what git would materialise for that path, write a single
/// stable advisory line to `notes`:
///
/// ```text
/// note: <path>: line endings (<actual>) differ from git's expected <expected> (per .gitattributes / core.autocrlf / core.eol); run 'tpu doctor' to normalize
/// ```
///
/// This is **best-effort and never fails a read**: any error opening the
/// repository, resolving attributes, or reading the file is swallowed and
/// produces no note.  The condition is unique to git EOL mismatches and is
/// distinct from the mojibake advisory.
///
/// Only a bounded prefix of the file is read (see [`GitEol::advisory_note`]),
/// so this does not turn a `head`/`tail` into a whole-file read.  Callers that
/// already hold an open [`GitEol`] (e.g. a long-lived server caching handles)
/// should call [`GitEol::advisory_note`] directly to avoid re-opening the repo.
pub fn emit_eol_advisory(
    notes: &mut dyn std::io::Write,
    git_root: &Path,
    file: &Path,
) -> std::io::Result<()> {
    if let Ok(Some(git)) = GitEol::open(git_root)
        && let Some(line) = git.advisory_note(file)
    {
        writeln!(notes, "{line}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// Create a fresh repository under a temp dir, optionally writing a
    /// `.gitattributes` file and appending `core` configuration.
    fn init_repo(gitattributes: Option<&str>, core_config: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        gix::init(dir.path()).expect("git init");

        if let Some(attrs) = gitattributes {
            fs::write(dir.path().join(".gitattributes"), attrs).expect("write .gitattributes");
        }
        if !core_config.is_empty() {
            let cfg_path = dir.path().join(".git").join("config");
            let mut cfg = fs::read_to_string(&cfg_path).unwrap_or_default();
            cfg.push_str("\n[core]\n");
            cfg.push_str(core_config);
            fs::write(&cfg_path, cfg).expect("write config");
        }
        dir
    }

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, bytes).expect("write file");
        p
    }

    #[test]
    fn no_attrs_no_autocrlf_means_no_expectation() {
        // Explicitly disable autocrlf at the repository level so the result is
        // deterministic regardless of any global `core.autocrlf` on the host.
        let dir = init_repo(None, "\tautocrlf = false\n");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let file = write_file(dir.path(), "mixed.txt", b"a\r\nb\nc\r\n");
        // autocrlf disabled, no attrs => binary digest => no normalisation.
        assert_eq!(git.detect(&file, b"a\r\nb\nc\r\n").unwrap(), None);
        assert_eq!(git.expected_line_ending(&file).unwrap(), None);
    }

    #[test]
    fn eol_lf_attribute_flags_crlf_file() {
        let dir = init_repo(Some("*.txt text eol=lf\n"), "");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let file = write_file(dir.path(), "a.txt", b"x\r\ny\r\n");
        let m = git.detect(&file, b"x\r\ny\r\n").unwrap().expect("mismatch");
        assert_eq!(m.expected, LineEnding::Lf);
        assert_eq!(m.actual, LineEnding::CrLf);
    }

    #[test]
    fn eol_lf_attribute_accepts_lf_file() {
        let dir = init_repo(Some("*.txt text eol=lf\n"), "");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let file = write_file(dir.path(), "a.txt", b"x\ny\n");
        assert_eq!(git.detect(&file, b"x\ny\n").unwrap(), None);
        assert_eq!(
            git.expected_line_ending(&file).unwrap(),
            Some(LineEnding::Lf)
        );
    }

    #[test]
    fn eol_crlf_attribute_flags_lf_file() {
        let dir = init_repo(Some("*.txt text eol=crlf\n"), "");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let file = write_file(dir.path(), "a.txt", b"x\ny\n");
        let m = git.detect(&file, b"x\ny\n").unwrap().expect("mismatch");
        assert_eq!(m.expected, LineEnding::CrLf);
        assert_eq!(m.actual, LineEnding::Lf);
    }

    #[test]
    fn autocrlf_true_expects_crlf_for_text() {
        let dir = init_repo(None, "\tautocrlf = true\n");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let file = write_file(dir.path(), "a.txt", b"x\ny\n");
        let m = git.detect(&file, b"x\ny\n").unwrap().expect("mismatch");
        assert_eq!(m.expected, LineEnding::CrLf);
        assert_eq!(m.actual, LineEnding::Lf);
    }

    #[test]
    fn autocrlf_true_leaves_binary_alone() {
        let dir = init_repo(None, "\tautocrlf = true\n");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let bytes = b"x\ny\0z\n";
        let file = write_file(dir.path(), "a.bin", bytes);
        assert_eq!(git.detect(&file, bytes).unwrap(), None);
    }

    #[test]
    fn explicit_minus_text_disables_normalisation() {
        let dir = init_repo(Some("*.bin -text\n"), "\tautocrlf = true\n");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let bytes = b"x\r\ny\nz\r\n";
        let file = write_file(dir.path(), "a.bin", bytes);
        assert_eq!(git.detect(&file, bytes).unwrap(), None);
    }

    #[test]
    fn path_outside_worktree_is_none() {
        let dir = init_repo(Some("*.txt text eol=lf\n"), "");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = write_file(outside.path(), "a.txt", b"x\r\n");
        assert_eq!(git.detect(&file, b"x\r\n").unwrap(), None);
    }

    #[test]
    fn free_detect_helper_matches() {
        let dir = init_repo(Some("*.txt text eol=lf\n"), "");
        let file = write_file(dir.path(), "a.txt", b"x\r\ny\r\n");
        let m = detect(dir.path(), &file, b"x\r\ny\r\n")
            .unwrap()
            .expect("mismatch");
        assert_eq!(m.expected, LineEnding::Lf);
    }

    #[test]
    fn dominant_line_ending_prefers_majority() {
        // Mixed file where git wants LF: the lone non-conforming ending is a
        // single CRLF amid many LFs, so the mismatch must report `actual =
        // CRLF` (the offender) and never `actual == expected`.
        let dir = init_repo(Some("*.txt text eol=lf\n"), "");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let bytes = b"a\nb\nc\nd\r\n";
        let file = write_file(dir.path(), "a.txt", bytes);
        let m = git.detect(&file, bytes).unwrap().expect("mismatch");
        assert_eq!(m.expected, LineEnding::Lf);
        assert_eq!(m.actual, LineEnding::CrLf);
        assert_ne!(m.actual, m.expected);
    }

    #[test]
    fn mostly_crlf_with_stray_lf_reports_lf_not_crlf() {
        // git wants CRLF; the file is predominantly CRLF but has a stray bare
        // LF.  The old code reported `actual = CRLF` (the dominant ending),
        // which equals `expected` and reads as a contradiction.  We must
        // report the offending `LF` instead.
        let dir = init_repo(Some("*.txt text eol=crlf\n"), "");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let bytes = b"a\r\nb\r\nc\r\nd\n";
        let file = write_file(dir.path(), "a.txt", bytes);
        let m = git.detect(&file, bytes).unwrap().expect("mismatch");
        assert_eq!(m.expected, LineEnding::CrLf);
        assert_eq!(m.actual, LineEnding::Lf);
    }

    #[test]
    fn lone_cr_conflicts_when_crlf_expected() {
        // git wants CRLF; a classic-Mac CR-only file must be flagged.  The
        // previous logic only checked for bare LFs and silently accepted lone
        // CRs.
        let dir = init_repo(Some("*.txt text eol=crlf\n"), "");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let bytes = b"a\rb\rc\r";
        let file = write_file(dir.path(), "a.txt", bytes);
        let m = git.detect(&file, bytes).unwrap().expect("mismatch");
        assert_eq!(m.expected, LineEnding::CrLf);
        assert_eq!(m.actual, LineEnding::Cr);
    }
}
