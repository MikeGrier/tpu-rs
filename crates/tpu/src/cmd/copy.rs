// Copyright (c) 2026, Michael Grier

//! `tpu copy` — encoding-preserving file copy with optional recursion and
//! resilient error handling.
//!
//! Supports three shapes of operation:
//!
//! - **single-file copy** — `tpu copy SRC DST` copies one file (or, when
//!   `DST` is a directory, copies into `DST/<src-filename>`);
//! - **recursive directory copy** — `tpu copy --recursive SRC DST` copies
//!   the contents of directory `SRC` into `DST`, creating intermediate
//!   directories as needed;
//! - **glob-driven copy** — when `SRC` contains `*`, `?`, `[`, or `{` it is
//!   expanded against the current directory and every matching file is
//!   copied into `DST` (which must be a directory).
//!
//! By default the walk continues past directories or files that cannot be
//! read; each problem produces a warning record (NDJSON) or a stderr note
//! (human mode). Pass [`OnError::Fail`] to abort on the first error.
//!
//! Bytes are copied verbatim — no encoding or line-ending transformation is
//! applied. This is a pure file-copy primitive intended for templating
//! scaffolds, mirroring fixtures, and similar low-risk operations.

use std::{
    fs,
    path::{Path, PathBuf},
};

use globset::Glob;
use walkdir::WalkDir;

use crate::shell::Shell;

/// Canonicalize a path, or — when it doesn't exist yet — canonicalize the
/// deepest existing ancestor and re-append the non-existing tail.
///
/// This normalises `..` components in the non-existing suffix so that a dest
/// like `other/../src/dst` is correctly identified as being inside `src/`.
fn canon_nearest(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return c;
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        let parent = match cur.parent() {
            Some(p) if p != cur => p.to_path_buf(),
            _ => break,
        };
        if let Some(name) = cur.file_name() {
            tail.push(name.to_owned());
        }
        cur = parent;
        if let Ok(c) = cur.canonicalize() {
            let mut result = c;
            for part in tail.into_iter().rev() {
                result.push(part);
            }
            return result;
        }
    }
    path.to_path_buf()
}

/// Result of one [`run`] invocation.
#[derive(Debug, Default)]
pub struct CopyReport {
    /// Files copied successfully.
    pub copied: usize,
    /// Files skipped because the destination already existed and
    /// `--overwrite` was not supplied.
    pub skipped: usize,
    /// Errors encountered while walking, reading, or writing.
    pub warnings: usize,
}

/// What to do when an individual entry fails (unreadable directory,
/// permission denied, glob walk error, target write error, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    /// Emit a warning and continue with the next entry. Default.
    Warn,
    /// Abort the entire copy on the first error.
    Fail,
}

impl Default for OnError {
    fn default() -> Self {
        OnError::Warn
    }
}

/// Options for [`run`].
#[derive(Debug, Default)]
pub struct CopyOptions {
    /// Recurse into directories. Required when `source` is a directory.
    pub recursive: bool,
    /// Overwrite existing destination files. Without this flag an existing
    /// file at the destination is skipped (and counted in
    /// [`CopyReport::skipped`]).
    pub overwrite: bool,
    /// Behaviour when an individual entry fails.
    pub on_error: OnError,
}

/// Copy `source` to `dest` according to `opts`.
///
/// Diagnostic output (warnings, per-file status) is routed through `shell` so
/// it honours the global `--message-format` selection.
pub fn run(
    source: &str,
    dest: &Path,
    opts: CopyOptions,
    shell: &mut Shell,
) -> Result<CopyReport, Box<dyn std::error::Error>> {
    let mut report = CopyReport::default();

    // Glob expansion: the source spec contains a glob meta-character.
    if is_glob(source) {
        let matcher = Glob::new(source)
            .map_err(|e| format!("copy: invalid glob {source:?}: {e}"))?
            .compile_matcher();

        // Destination must be a directory (or be created as one) so that
        // every glob match has a unique target.
        if dest.exists() && !dest.is_dir() {
            return Err(format!(
                "copy: glob source requires DEST to be a directory: {}",
                dest.display()
            )
            .into());
        }
        fs::create_dir_all(dest).map_err(|e| {
            format!("copy: cannot create destination {}: {e}", dest.display())
        })?;

        // Walk from the appropriate root: for absolute patterns the anchor
        // directory (longest non-glob prefix) is used so that entry paths are
        // absolute and the matcher can compare them against the full pattern.
        let first_meta = source.bytes().position(|b| b"*?[{".contains(&b));
        let anchor_str = first_meta.map(|i| &source[..i]).unwrap_or(source);
        let anchor_path = Path::new(anchor_str);
        let (walk_root, absolute_walk) = if anchor_path.is_absolute() {
            let root = anchor_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(anchor_path);
            (root.to_path_buf(), true)
        } else {
            (PathBuf::from("."), false)
        };
        for entry in WalkDir::new(&walk_root) {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    report.warnings += 1;
                    if matches!(opts.on_error, OnError::Fail) {
                        return Err(format!("copy: glob walk: {e}").into());
                    }
                    let _ = shell.warn(format!("copy: glob walk: {e}"));
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let match_path = if absolute_walk {
                entry.path()
            } else {
                entry.path().strip_prefix(".").unwrap_or(entry.path())
            };
            if !matcher.is_match(match_path) {
                continue;
            }
            let leaf = match entry.path().file_name() {
                Some(n) => n,
                None => continue,
            };
            let target = dest.join(leaf);
            copy_one(entry.path(), &target, &opts, shell, &mut report)?;
        }
        return Ok(report);
    }

    let src_path = Path::new(source);

    let metadata = match src_path.metadata() {
        Ok(m) => m,
        Err(e) => return Err(format!("copy: source {}: {e}", src_path.display()).into()),
    };

    if metadata.is_dir() {
        if !opts.recursive {
            return Err(format!(
                "copy: source {} is a directory; pass --recursive to copy directory trees",
                src_path.display()
            )
            .into());
        }
        // Guard against copying a tree into itself: dest inside src would
        // recurse indefinitely and exhaust disk space or path depth.
        {
            let src_canon = fs::canonicalize(src_path)
                .map_err(|e| format!("copy: source {}: {e}", src_path.display()))?;
            let dest_abs = if dest.is_absolute() {
                dest.to_path_buf()
            } else {
                std::env::current_dir()
                    .map_err(|e| format!("copy: cwd: {e}"))?
                    .join(dest)
            };
            let dest_canon = canon_nearest(&dest_abs);
            if dest_canon == src_canon || dest_canon.starts_with(&src_canon) {
                return Err(format!(
                    "copy: destination {} is inside source {}; \
                     recursive copy would loop indefinitely",
                    dest.display(),
                    src_path.display()
                )
                .into());
            }
        }
        // Directory copy: walk SRC, mirror structure under DEST.
        fs::create_dir_all(dest).map_err(|e| {
            format!("copy: cannot create destination {}: {e}", dest.display())
        })?;
        let walker = WalkDir::new(src_path).into_iter();
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    report.warnings += 1;
                    if matches!(opts.on_error, OnError::Fail) {
                        return Err(format!("copy: walk: {e}").into());
                    }
                    let path_hint = e
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "?".to_string());
                    let _ = shell.warn(format!("copy: cannot access {path_hint}: {e}"));
                    continue;
                }
            };
            let rel = match entry.path().strip_prefix(src_path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if rel.as_os_str().is_empty() {
                // The walk root itself.
                continue;
            }
            let target = dest.join(rel);
            if entry.file_type().is_dir() {
                if let Err(e) = fs::create_dir_all(&target) {
                    report.warnings += 1;
                    if matches!(opts.on_error, OnError::Fail) {
                        return Err(
                            format!("copy: mkdir {}: {e}", target.display()).into()
                        );
                    }
                    let _ = shell.warn(format!("copy: mkdir {}: {e}", target.display()));
                }
            } else if entry.file_type().is_file() {
                copy_one(entry.path(), &target, &opts, shell, &mut report)?;
            }
            // Symlinks and other entry types are silently ignored — keep
            // the surface area small until there is a documented need.
        }
        return Ok(report);
    }

    // Single-file copy. If DEST is an existing directory, copy into it
    // under the source filename.
    let target: PathBuf = if dest.is_dir() {
        match src_path.file_name() {
            Some(n) => dest.join(n),
            None => return Err(format!("copy: source has no filename: {}", src_path.display()).into()),
        }
    } else {
        dest.to_path_buf()
    };
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                format!("copy: cannot create parent {}: {e}", parent.display())
            })?;
        }
    }
    copy_one(src_path, &target, &CopyOptions { on_error: OnError::Fail, ..opts }, shell, &mut report)?;
    Ok(report)
}

fn copy_one(
    src: &Path,
    dst: &Path,
    opts: &CopyOptions,
    shell: &mut Shell,
    report: &mut CopyReport,
) -> Result<(), Box<dyn std::error::Error>> {
    // Same-file guard: silently skip if src and dst resolve to the same path,
    // rather than truncating the file by overwriting it with itself.
    if let (Ok(a), Ok(b)) = (src.canonicalize(), dst.canonicalize()) {
        if a == b {
            report.skipped += 1;
            return Ok(());
        }
    }
    if dst.exists() && !opts.overwrite {
        report.skipped += 1;
        return Ok(());
    }
    match fs::copy(src, dst) {
        Ok(_) => {
            report.copied += 1;
            Ok(())
        }
        Err(e) => {
            report.warnings += 1;
            let msg = format!("copy: {} -> {}: {e}", src.display(), dst.display());
            if matches!(opts.on_error, OnError::Fail) {
                return Err(msg.into());
            }
            let _ = shell.warn(msg);
            Ok(())
        }
    }
}

fn is_glob(spec: &str) -> bool {
    spec.contains('*') || spec.contains('?') || spec.contains('[') || spec.contains('{')
}
