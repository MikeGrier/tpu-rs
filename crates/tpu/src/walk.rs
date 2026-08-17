// Copyright (c) 2026, Michael Grier

//! Directory enumeration and glob matching built on the `globazog` engine.
//!
//! This module replaces the previous `walkdir` + `globset` pairing used by the
//! `find`, `copy`, and `doctor` commands. Every traversal runs with globazog's
//! default [`FollowLinks::Never`](globazog::FollowLinks) policy, so symbolic
//! links and Windows junctions / reparse points are **never** descended into.
//!
//! Two surfaces are exposed:
//!
//! - [`walk`] — enumerate a directory tree, returning the files that match a
//!   glob pattern, as paths relative to the walk root. Callers join the root
//!   back on to obtain full paths (which preserves the caller's original —
//!   possibly relative — root prefix).
//! - [`walk_each`] — the same traversal, but delivered entry-by-entry
//!   (directories and files) to a callback so callers processing very large
//!   trees (e.g. recursive copy) need not materialize the whole listing first.
//! - [`GlobMatcher`] — a standalone glob matcher over already-known relative
//!   paths (used for `.gitignore` filtering), replacing `globset::GlobSet`.

use std::{
    collections::HashMap,
    error::Error,
    path::{Path, PathBuf},
};

use globazog::{
    CaseSensitivity, ContainerId, ContainerName, CqItem, Dialect, EntryType, Leaf, MetaMask,
    QueryBuilder,
    syntax::{CodePoint, Segment, Token, set::PatternSet},
};

use crate::cmd::copy::OnError;

/// The result of a [`walk`]: the files (relative to the walk root) that
/// matched the glob pattern.
#[derive(Debug, Default)]
pub struct Walk {
    /// Regular files whose path (relative to the walk root) matched the glob
    /// pattern. Symbolic links and reparse points are excluded.
    pub files: Vec<PathBuf>,
}

/// One entry delivered by [`walk_each`]: a path relative to the walk root plus
/// its kind.
#[derive(Debug)]
pub enum Entry {
    /// A directory the walk entered (including empty ones).
    Dir(PathBuf),
    /// A regular file matching the pattern. Symbolic links and reparse points
    /// are excluded.
    File(PathBuf),
}

/// Walk `root` with globazog, collecting the files that match `pattern`.
///
/// `pattern` is a [`Dialect::Posix`] glob (case-sensitive, `/` separators,
/// `*` / `?` / `**` / `{a,b}`) matched against each entry's path relative to
/// `root`. `skip_dirs` lists directory *names* that must not be descended into
/// (e.g. `.git`, `node_modules`, `target`); pass an empty slice to descend
/// everywhere. Symbolic links and junctions are never followed regardless.
///
/// Per-entry failures are handled per `on_error`: [`OnError::Fail`] aborts with
/// an error, while [`OnError::Warn`] appends a `"{label}: cannot access …"`
/// note to `warnings` and continues. `label` prefixes those notes and the
/// fatal error message (e.g. `"find"`, `"copy"`, `"doctor"`).
///
/// This buffers the whole listing; use [`walk_each`] to stream entries.
pub fn walk(
    root: &Path,
    pattern: &str,
    skip_dirs: &[&str],
    on_error: OnError,
    label: &str,
    warnings: &mut Vec<String>,
) -> Result<Walk, Box<dyn Error>> {
    let mut out = Walk::default();
    walk_each(
        root,
        pattern,
        skip_dirs,
        on_error,
        label,
        warnings,
        |entry| {
            if let Entry::File(p) = entry {
                out.files.push(p);
            }
            Ok(())
        },
    )?;
    Ok(out)
}

/// Split a glob `spec` into a physical walk root and the pattern relative to
/// that root, so it can be handed to [`walk`] / [`walk_each`] (whose patterns
/// are matched relative to the root).
///
/// An **absolute** spec anchors at the longest non-glob prefix directory
/// (e.g. `/repo/src/**/*.rs` → root `/repo/src`, pattern `**/*.rs`), which
/// keeps it working regardless of `/` vs `\` separators. A **relative** spec
/// walks from `.` with the spec used verbatim. Rejoin `root.join(rel)` on each
/// result to reconstruct full paths.
pub fn split_glob_root(spec: &str) -> (PathBuf, String) {
    let first_meta = spec.bytes().position(|b| b"*?[{".contains(&b));
    let anchor_str = first_meta.map(|i| &spec[..i]).unwrap_or(spec);
    let anchor_path = Path::new(anchor_str);
    if !anchor_path.is_absolute() {
        return (PathBuf::from("."), spec.to_string());
    }
    // When the anchor already ends with a separator it is itself the directory
    // to search; otherwise its parent is (the trailing element is a partial
    // file/dir name that belongs to the pattern).
    let ends_with_sep =
        anchor_str.ends_with('/') || anchor_str.ends_with(std::path::MAIN_SEPARATOR);
    let root = if ends_with_sep {
        anchor_path.to_path_buf()
    } else {
        anchor_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(anchor_path)
            .to_path_buf()
    };
    // The pattern is everything after the final separator of the non-glob
    // prefix. Slicing by prefix length (rather than a string strip against the
    // rendered root) stays correct regardless of `/` vs `\` separators.
    let rel_start = anchor_str.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
    (root, spec[rel_start..].to_string())
}

/// Like [`walk`] but invokes `sink` for each [`Entry`] as it is discovered,
/// in traversal order (a directory is delivered before any entry inside it),
/// instead of buffering the whole listing. If `sink` returns an error the walk
/// stops and that error is propagated.
pub fn walk_each(
    root: &Path,
    pattern: &str,
    skip_dirs: &[&str],
    on_error: OnError,
    label: &str,
    warnings: &mut Vec<String>,
    mut sink: impl FnMut(Entry) -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    // globazog roots must be absolute (D-74); resolve a relative root against
    // the current directory. Path reconstruction below is independent of this
    // absolute form — it rebuilds paths relative to the root.
    let root_abs = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("{label}: cwd: {e}"))?
            .join(root)
    };

    // Descend only into real directories that are not on the skip list. The
    // reparse guard is belt-and-suspenders over the Never follow policy.
    let mut descend = vec![Leaf::IsReparse { negate: true }];
    for &d in skip_dirs {
        descend.push(Leaf::Name {
            seg: literal_segment(d),
            case: CaseSensitivity::Sensitive,
            negate: true,
        });
    }

    let handle = QueryBuilder::new()
        .root(root_abs)
        .pattern(pattern, Dialect::Posix, Vec::new())
        .descend(descend)
        // TYPE + REPARSE so each match reports whether it is a real file.
        .result_shape(MetaMask::TYPE | MetaMask::REPARSE)
        .submit()
        .map_err(|e| format!("{label}: invalid glob {pattern:?}: {e}"))?;

    let ring = handle.completions();
    // ContainerId -> (parent, own name); a root's name is `None`.
    let mut containers: HashMap<ContainerId, (Option<ContainerId>, Option<String>)> =
        HashMap::new();

    loop {
        match ring.wait_pop() {
            CqItem::ContainerEnter(e) => {
                let name = match e.name {
                    ContainerName::Entry(n) => Some(n.to_string_lossy()),
                    ContainerName::Root(_) => None,
                };
                containers.insert(e.id, (e.parent, name));
                if let Some(rel) = container_rel(e.id, &containers)
                    && !rel.as_os_str().is_empty()
                {
                    sink(Entry::Dir(rel))?;
                }
            }
            CqItem::Match(m) => {
                // Only real files, mirroring walkdir's `file_type().is_file()`
                // (symlinks / reparse points are skipped).
                if m.meta.entry_type != EntryType::File || m.meta.is_reparse {
                    continue;
                }
                let base = container_rel(m.container, &containers).unwrap_or_default();
                sink(Entry::File(base.join(m.name.to_string_lossy())))?;
            }
            CqItem::Error(e) => {
                // The best-available entry name, included in both the fatal
                // (Fail) and non-fatal (Warn) messages so failures are
                // actionable and the two paths stay consistent.
                let hint = e
                    .error
                    .name
                    .as_ref()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_else(|| "?".to_string());
                match on_error {
                    OnError::Fail => {
                        return Err(
                            format!("{label}: cannot access {hint}: {}", e.error.source).into()
                        );
                    }
                    OnError::Warn => {
                        warnings.push(format!("{label}: cannot access {hint}: {}", e.error.source));
                    }
                }
            }
            CqItem::Terminal(_) => break,
            _ => {}
        }
    }

    Ok(())
}

/// Rebuild a container's path relative to the walk root by walking the
/// parent chain up to the root (whose stored name is `None`).
fn container_rel(
    id: ContainerId,
    containers: &HashMap<ContainerId, (Option<ContainerId>, Option<String>)>,
) -> Option<PathBuf> {
    let mut parts: Vec<&str> = Vec::new();
    let mut cur = id;
    loop {
        let (parent, name) = containers.get(&cur)?;
        match name {
            Some(n) => parts.push(n),
            None => break, // reached the root
        }
        match parent {
            Some(p) => cur = *p,
            None => break,
        }
    }
    let mut rel = PathBuf::new();
    for p in parts.iter().rev() {
        rel.push(p);
    }
    Some(rel)
}

/// Build a globazog [`Segment`] that matches `name` literally: every character
/// becomes a [`Token::Literal`], so a skip-dir name containing glob
/// metacharacters (or a construct the parser rejects) can never broaden or
/// narrow the descend filter.
fn literal_segment(name: &str) -> Segment {
    name.chars()
        .map(|c| Token::Literal(c as CodePoint))
        .collect()
}

/// A set of globs matched against already-known relative paths — the
/// replacement for `globset::GlobSet` used by `.gitignore` filtering.
#[derive(Debug, Default)]
pub struct GlobMatcher {
    set: PatternSet,
}

impl GlobMatcher {
    /// An empty matcher.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a [`Dialect::Posix`] glob. Returns `false` (and adds nothing) when
    /// the pattern fails to compile — e.g. it uses an unsupported construct.
    pub fn add(&mut self, pattern: &str) -> bool {
        self.set.add(pattern, Dialect::Posix, None).is_ok()
    }

    /// Whether any held glob matches `rel` (matched segment-by-segment, on
    /// either `/` or `\` separators).
    pub fn is_match(&self, rel: &Path) -> bool {
        let owned: Vec<Vec<CodePoint>> = rel
            .to_string_lossy()
            .split(['/', '\\'])
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().map(|c| c as CodePoint).collect())
            .collect();
        let segs: Vec<&[CodePoint]> = owned.iter().map(|v| v.as_slice()).collect();
        !self.set.matches(&segs).is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_relative_glob_walks_dot() {
        let (root, pat) = split_glob_root("sub/*.rs");
        assert_eq!(root, PathBuf::from("."));
        assert_eq!(pat, "sub/*.rs");
    }

    #[test]
    fn split_absolute_glob_anchors_at_prefix() {
        let base = if cfg!(windows) {
            "C:/repo/src"
        } else {
            "/repo/src"
        };
        let (root, pat) = split_glob_root(&format!("{base}/**/*.rs"));
        assert_eq!(root, PathBuf::from(base));
        assert_eq!(pat, "**/*.rs");
    }

    #[test]
    fn split_absolute_glob_with_partial_leaf() {
        let base = if cfg!(windows) { "C:/repo" } else { "/repo" };
        let (root, pat) = split_glob_root(&format!("{base}/foo*.rs"));
        assert_eq!(root, PathBuf::from(base));
        assert_eq!(pat, "foo*.rs");
    }
}
