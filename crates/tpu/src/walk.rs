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
//!   glob pattern and the directories that were entered, as paths relative to
//!   the walk root. Callers join the root back on to obtain full paths (which
//!   preserves the caller's original — possibly relative — root prefix).
//! - [`walk_each`] — the same traversal, but delivered entry-by-entry to a
//!   callback so callers processing very large trees (e.g. recursive copy)
//!   need not materialize the whole listing first.
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

/// Walk `root` with globazog, collecting files that match `pattern` and the
/// directories that were entered.
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
