// Copyright (c) 2026, Michael Grier

//! Git-aware worktree policy using gitoxide (pure Rust, no `git` binary).
//!
//! This module answers a single question: *does a file's on-disk line-ending
//! convention match what git would materialise in the working tree* for that
//! path, given the repository's `.gitattributes` rules and the `core.autocrlf`
//! / `core.eol` configuration?
//!
//! Repository discovery is automatic and cached by directory.  This makes
//! multi-file and glob operations pay the repository-open cost at most once
//! per worktree, including when a glob root sits above multiple repositories.
//!
//! ## Multi-file processing: resolve sequentially, process in parallel
//!
//! Commands that operate on many files (a glob, a directory walk) must not
//! call back into this module's gitoxide-backed functions
//! ([`policy_for_path`], [`detect_for_path`], [`GitEol::open`], …) from
//! multiple threads at once — `gix::Repository` is not required to be `Sync`,
//! and doing so would also reopen repositories redundantly.  Instead:
//!
//! 1. Enumerate candidate paths (no gitoxide involved).
//! 2. Resolve each path's policy **sequentially, on a single thread**, via
//!    [`resolve_policies`]. This is where every gitoxide call in the whole
//!    operation happens, and it needs no locking because it never shares
//!    gitoxide state across threads.
//! 3. Hand the resulting owned, `Send + Sync` [`FilePolicy`] values to
//!    per-file processing, which may run in parallel. Workers determine EOL
//!    mismatches with the pure [`detect_with_policy`] and never touch
//!    gitoxide again.
//!
//! See [`resolve_policies`] for the full contract, including the consistency
//! tradeoff this implies (no whole-batch `.gitattributes` snapshot).
//!
//! The expected working-tree line ending is computed exactly the way git (and
//! [`gix_filter`]) computes it: the `text`, `crlf` and `eol` attributes are
//! combined into an [`AttributesDigest`], folded together with the repository's
//! [`Configuration`] (`core.autocrlf` / `core.eol`), and reduced to a target
//! [`Mode`] (LF or CRLF) — or to "no normalisation" for binary / `-text`
//! paths.  The resolved `working-tree-encoding` attribute is also exposed so
//! callers can decode and encode the worktree representation deterministically.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
};

use gix::filter::plumbing::eol::{AttributesDigest, Configuration, Mode, Stats};
use harrier::encoding::{BomPolicy as SourceBomPolicy, LineEnding, SourceConfig};
use redwing::Branch;

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

/// BOM constraint implied by a `working-tree-encoding` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeBom {
    /// The attribute does not impose a BOM requirement.
    Unspecified,
    /// The encoded worktree file must begin with this BOM.
    Required(&'static [u8]),
    /// `working-tree-encoding=UTF-16` accepts either UTF-16 byte order, but
    /// requires a BOM to identify it.
    RequiredUtf16,
    /// The named encoding is explicitly the BOM-less Git spelling.
    Forbidden,
}

/// A resolved Git worktree encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEncoding {
    /// Original `.gitattributes` value, retained for diagnostics.
    pub label: String,
    /// `encoding_rs` encoding used for worktree reads and writes.
    pub encoding: &'static encoding_rs::Encoding,
    /// BOM constraint encoded by Git's UTF-16/`-BOM` spellings.
    pub bom: WorktreeBom,
}

/// Git policy resolved for one worktree path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilePolicy {
    /// Definite line ending Git would materialise, when one is configured.
    pub line_ending: Option<LineEnding>,
    /// Declared worktree encoding, when `working-tree-encoding` applies.
    pub working_tree_encoding: Option<WorktreeEncoding>,
    /// `true` when `text=auto` applies: binary-looking content is exempt
    /// from `line_ending` normalisation (see
    /// [`FilePolicy::line_ending_for_text`]).
    pub auto_text: bool,
}

impl FilePolicy {
    pub(crate) fn source_config(&self, line_ending: Option<LineEnding>) -> SourceConfig {
        let mut config = SourceConfig::default();
        config.line_ending_default = line_ending;
        if let Some(worktree) = &self.working_tree_encoding {
            config.encoding_hint = Some(worktree.encoding);
            config.bom_policy = match worktree.bom {
                WorktreeBom::Required(_) | WorktreeBom::RequiredUtf16 => SourceBomPolicy::Honour,
                WorktreeBom::Forbidden => SourceBomPolicy::Ignore,
                WorktreeBom::Unspecified if worktree.encoding == encoding_rs::UTF_8 => {
                    SourceBomPolicy::Honour
                }
                WorktreeBom::Unspecified => SourceBomPolicy::Ignore,
            };
        }
        config
    }

    /// Resolve conditional `text=auto` EOL policy against encoded worktree
    /// bytes. Explicit `text`/`eol` policy is always returned.
    pub fn line_ending_for_worktree_bytes(&self, bytes: &[u8]) -> Option<LineEnding> {
        let analysis = eol_analysis_bytes(bytes, self.working_tree_encoding.as_ref());
        self.line_ending_for_analysis_bytes(&analysis)
    }

    /// Resolve conditional `text=auto` EOL policy against decoded UTF-8 text.
    pub fn line_ending_for_text(&self, text: &str) -> Option<LineEnding> {
        self.line_ending_for_analysis_bytes(text.as_bytes())
    }

    fn line_ending_for_analysis_bytes(&self, bytes: &[u8]) -> Option<LineEnding> {
        if self.auto_text && Stats::from_bytes(bytes).is_binary() {
            None
        } else {
            self.line_ending
        }
    }

    pub(crate) fn validate_branch(&self, file: &Path, branch: &dyn Branch) -> Result<(), BoxError> {
        let Some(worktree) = &self.working_tree_encoding else {
            return Ok(());
        };
        let prefix_len = branch.byte_len().min(3) as usize;
        let mut prefix = vec![0; prefix_len];
        if prefix_len > 0 {
            branch.read_at(0, &mut prefix)?;
        }
        validate_worktree_bom(file, worktree, &prefix, branch.byte_len())
    }

    /// Decide whether a preserving write should emit a BOM.
    pub fn write_bom(&self, source_had_bom: bool) -> bool {
        match self
            .working_tree_encoding
            .as_ref()
            .map(|worktree| worktree.bom)
        {
            Some(WorktreeBom::Required(_)) | Some(WorktreeBom::RequiredUtf16) => true,
            Some(WorktreeBom::Forbidden) => false,
            Some(WorktreeBom::Unspecified) | None => source_had_bom,
        }
    }
}

#[derive(Debug)]
struct ResolvedAttributes {
    digest: AttributesDigest,
    working_tree_encoding: Option<WorktreeEncoding>,
}

#[derive(Default)]
struct DiscoveryCache {
    directory_roots: HashMap<PathBuf, Option<PathBuf>>,
    repositories: HashMap<PathBuf, Rc<GitEol>>,
}

thread_local! {
    static DISCOVERY_CACHE: RefCell<DiscoveryCache> = RefCell::new(DiscoveryCache::default());
}

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
        Self::from_repository(repo)
    }

    fn from_repository(repo: gix::Repository) -> Result<Option<Self>, BoxError> {
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
        Ok(Some(Self {
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
    ///
    /// This queries gitoxide (`attributes_for`) on every call. Callers
    /// processing many files should instead resolve a [`FilePolicy`] once per
    /// file up front (see [`resolve_policies`]) and call the pure
    /// [`detect_with_policy`] function against it — that avoids a gitoxide
    /// call per file entirely.
    pub fn detect(&self, file: &Path, bytes: &[u8]) -> Result<Option<EolMismatch>, BoxError> {
        let Some(policy) = self.file_policy(file)? else {
            return Ok(None);
        };
        Ok(detect_with_policy(&policy, bytes))
    }

    /// The git line ending git expects in the working tree for `file`, or
    /// `None` if git would not normalise this path.
    pub fn expected_line_ending(&self, file: &Path) -> Result<Option<LineEnding>, BoxError> {
        let Some(attributes) = self.attributes_for(file)? else {
            return Ok(None);
        };
        Ok(attributes
            .digest
            .to_eol(self.config)
            .map(mode_to_line_ending))
    }

    /// Resolve all Git worktree policy TPU consumes for `file`.
    pub fn file_policy(&self, file: &Path) -> Result<Option<FilePolicy>, BoxError> {
        let Some(attributes) = self.attributes_for(file)? else {
            return Ok(None);
        };
        Ok(Some(FilePolicy {
            line_ending: attributes
                .digest
                .to_eol(self.config)
                .map(mode_to_line_ending),
            working_tree_encoding: attributes.working_tree_encoding,
            auto_text: attributes.digest.is_auto_text(),
        }))
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
    fn attributes_for(&self, file: &Path) -> Result<Option<ResolvedAttributes>, BoxError> {
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
        // Selection order is significant.
        outcome.initialize_with_selection(
            &Default::default(),
            ["crlf", "eol", "text", "working-tree-encoding"],
        );
        platform.matching_attributes(&mut outcome);

        let selected: Vec<_> = outcome.iter_selected().collect();
        let crlf_attr = selected[0].assignment.state;
        let eol_attr = selected[1].assignment.state;
        let text_attr = selected[2].assignment.state;
        let encoding_attr = selected[3].assignment.state;

        Ok(Some(ResolvedAttributes {
            digest: fold_digest(text_attr, crlf_attr, eol_attr, self.config),
            working_tree_encoding: parse_worktree_encoding(encoding_attr)?,
        }))
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

/// Resolve Git policy using the nearest containing repository.
///
/// Discovery and opened repository handles are cached for the current thread,
/// so a glob spanning many files opens each worktree only once. Policy is
/// resolved independently for each call; there is no operation-wide
/// `.gitattributes` snapshot, and concurrent attribute changes may therefore
/// affect later paths without changing policy already returned for earlier
/// paths.
pub fn policy_for_path(file: &Path) -> Result<FilePolicy, BoxError> {
    let Some(repository) = repository_for_path(file)? else {
        return Ok(FilePolicy::default());
    };
    Ok(repository.file_policy(file)?.unwrap_or_default())
}

/// Start a new top-level operation with a fresh repository-discovery cache.
///
/// Long-lived callers should invoke this once per request or glob. Discovery
/// and repository handles are then reused for every path in that operation,
/// while repositories or Git configuration created between operations are
/// noticed promptly.
pub fn begin_operation() {
    DISCOVERY_CACHE.with(|cache| {
        *cache.borrow_mut() = DiscoveryCache::default();
    });
}

/// One path's outcome from [`resolve_policies`]: either its resolved
/// [`FilePolicy`], or a description of why resolution failed for that path
/// specifically.
///
/// A resolution failure is deliberately **not** a `BoxError` here: it is
/// meant to be carried alongside successes in a plain `Vec` and consumed by
/// code that is not itself fallible (e.g. a per-file warning sink), and
/// `Box<dyn Error>` is neither `Clone` nor trivially comparable in tests.
#[derive(Debug, Clone)]
pub struct ResolvedPolicy {
    /// The path this policy was resolved for, exactly as supplied to
    /// [`resolve_policies`].
    pub path: PathBuf,
    /// `Ok` policy, or `Err` with a human-readable description of the
    /// resolution failure for this path.
    pub policy: Result<FilePolicy, String>,
}

/// Resolve Git policy for every path in `paths`, sequentially, in the order
/// given.
///
/// # The enumerate → resolve → parallelize model
///
/// This is the **sequential enrichment phase** of the pipeline multi-file
/// commands (`tpu doctor`, and any future glob-based mutator) are expected to
/// follow:
///
/// 1. **Enumerate.** A directory/glob walk (e.g. [`crate::walk::walk`])
///    produces the list — or, for a streaming enumerator, a queue — of
///    candidate paths. This step does not touch gitoxide at all.
/// 2. **Resolve, sequentially, on a single thread.** This function (or an
///    equivalent single-consumer loop draining an enumeration queue) walks
///    that list and calls [`policy_for_path`] once per path. All gitoxide
///    state — repository handles, attribute stacks, the directory→root
///    cache — is owned exclusively by this one call in this one thread for
///    its whole duration, so **no locking is required**: gitoxide's
///    `gix::Repository` is not required to be `Sync`, and this function never
///    shares it across threads.
/// 3. **Delegate to parallel workers.** The `Vec<ResolvedPolicy>` returned
///    here is plain, owned, `Send + Sync` data — no gitoxide handle, no
///    reference back into this module's caches. Per-file processing (mojibake
///    scanning, EOL repair, or any future per-file mutation) can therefore run
///    across as many threads as desired, each consuming only its own
///    `FilePolicy` values. Use the pure [`detect_with_policy`] (and
///    [`FilePolicy::line_ending_for_text`] /
///    [`FilePolicy::line_ending_for_worktree_bytes`]) from those workers —
///    **do not** call [`policy_for_path`], [`detect_for_path`], or
///    [`GitEol::open`] from a parallel worker; doing so would reopen
///    repositories redundantly and reintroduce the need for synchronisation
///    this split is meant to avoid.
///
/// # Consistency
///
/// Policy is resolved independently, path by path, in enumeration order.
/// There is **no whole-batch snapshot or lock** of `.gitattributes`: if
/// another process edits attributes while this function is still resolving
/// later paths, earlier and later paths in the same call may observe
/// different policy. This is an accepted, intentional tradeoff. Snapshotting
/// or locking repository state to defend against an attribute edit racing
/// with a single `tpu` invocation is disproportionate to how often that race
/// matters in practice; callers should not rely on every path in one call
/// sharing identical policy.
///
/// A resolution failure for one path (a corrupt `.git` directory, an
/// unreadable `.gitattributes`, an unsupported `working-tree-encoding`, …) is
/// recorded as `Err` on that path's [`ResolvedPolicy`] only. It never aborts
/// the batch, never panics, and never silently substitutes a default for a
/// path that failed — callers decide (typically: emit a warning and fall
/// back to [`FilePolicy::default`]) how to handle it. This function itself
/// cannot crash the process: every path either resolves or is reported.
pub fn resolve_policies(paths: impl IntoIterator<Item = PathBuf>) -> Vec<ResolvedPolicy> {
    paths
        .into_iter()
        .map(|path| {
            let policy = policy_for_path(&path).map_err(|e| e.to_string());
            ResolvedPolicy { path, policy }
        })
        .collect()
}

/// Detect an EOL mismatch using the nearest containing repository.
pub fn detect_for_path(file: &Path, bytes: &[u8]) -> Result<Option<EolMismatch>, BoxError> {
    let Some(repository) = repository_for_path(file)? else {
        return Ok(None);
    };
    repository.detect(file, bytes)
}

/// Return an advisory note using automatic nearest-repository discovery.
pub fn advisory_note_for_path(file: &Path) -> Option<String> {
    repository_for_path(file)
        .ok()
        .flatten()
        .and_then(|repository| repository.advisory_note(file))
}

fn repository_for_path(file: &Path) -> Result<Option<Rc<GitEol>>, BoxError> {
    let Some(root) = discover_worktree_root(file)? else {
        return Ok(None);
    };
    DISCOVERY_CACHE.with(|cache| {
        if let Some(repository) = cache.borrow().repositories.get(&root).cloned() {
            return Ok(Some(repository));
        }
        let Some(repository) = GitEol::open(&root)? else {
            return Ok(None);
        };
        let repository = Rc::new(repository);
        cache
            .borrow_mut()
            .repositories
            .insert(root, Rc::clone(&repository));
        Ok(Some(repository))
    })
}

fn discover_worktree_root(file: &Path) -> Result<Option<PathBuf>, BoxError> {
    let start = nearest_existing_directory(file)?;
    DISCOVERY_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let mut visited = Vec::new();
        let mut current = Some(start.as_path());
        let root = loop {
            let Some(directory) = current else {
                break None;
            };
            if let Some(cached) = cache.directory_roots.get(directory) {
                break cached.clone();
            }
            if directory.join(".git").try_exists()? {
                break Some(directory.to_path_buf());
            }
            visited.push(directory.to_path_buf());
            current = directory.parent();
        };
        cache.directory_roots.insert(start, root.clone());
        for directory in visited {
            cache.directory_roots.insert(directory, root.clone());
        }
        Ok(root)
    })
}

fn nearest_existing_directory(file: &Path) -> Result<PathBuf, BoxError> {
    let absolute = if file.is_absolute() {
        file.to_path_buf()
    } else {
        std::env::current_dir()?.join(file)
    };
    let mut candidate = if absolute.is_dir() {
        absolute
    } else {
        absolute
            .parent()
            .ok_or_else(|| format!("git: {} has no parent directory", file.display()))?
            .to_path_buf()
    };
    while !candidate.is_dir() {
        candidate = candidate
            .parent()
            .ok_or_else(|| format!("git: no existing ancestor directory for {}", file.display()))?
            .to_path_buf();
    }
    Ok(std::fs::canonicalize(&candidate).unwrap_or(candidate))
}

fn parse_worktree_encoding(
    state: gix::attrs::StateRef<'_>,
) -> Result<Option<WorktreeEncoding>, BoxError> {
    use gix::attrs::StateRef;

    let StateRef::Value(value) = state else {
        return match state {
            StateRef::Unspecified | StateRef::Unset => Ok(None),
            StateRef::Set => Err("working-tree-encoding must have an encoding value".into()),
            StateRef::Value(_) => unreachable!(),
        };
    };
    let label = std::str::from_utf8(value.as_bstr().as_ref())
        .map_err(|_| "working-tree-encoding is not valid UTF-8")?
        .to_owned();
    let upper = label.to_ascii_uppercase();
    let (encoding_label, bom) = match upper.as_str() {
        "UTF-16" => ("UTF-16LE", WorktreeBom::RequiredUtf16),
        "UTF-16LE-BOM" => ("UTF-16LE", WorktreeBom::Required(&[0xFF, 0xFE])),
        "UTF-16BE-BOM" => ("UTF-16BE", WorktreeBom::Required(&[0xFE, 0xFF])),
        "UTF-16LE" | "UTF-16BE" => (upper.as_str(), WorktreeBom::Forbidden),
        "UTF-8-BOM" => ("UTF-8", WorktreeBom::Required(&[0xEF, 0xBB, 0xBF])),
        "LATIN-1" | "LATIN1" | "ISO-8859-1" => {
            return Err(format!(
                "working-tree-encoding={label}: ISO-8859-1 is not supported losslessly"
            )
            .into());
        }
        _ => (label.as_str(), WorktreeBom::Unspecified),
    };
    let encoding = encoding_rs::Encoding::for_label(encoding_label.as_bytes())
        .ok_or_else(|| format!("working-tree-encoding={label}: encoding is not known"))?;
    Ok(Some(WorktreeEncoding {
        label,
        encoding,
        bom,
    }))
}

fn eol_analysis_bytes<'a>(
    bytes: &'a [u8],
    worktree: Option<&WorktreeEncoding>,
) -> std::borrow::Cow<'a, [u8]> {
    let (encoding, bom_len) = if bytes.starts_with(&[0xFF, 0xFE]) {
        (Some(encoding_rs::UTF_16LE), 2)
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        (Some(encoding_rs::UTF_16BE), 2)
    } else {
        (worktree.map(|worktree| worktree.encoding), 0)
    };
    let Some(encoding) = encoding.filter(|encoding| {
        *encoding == encoding_rs::UTF_16LE || *encoding == encoding_rs::UTF_16BE
    }) else {
        return std::borrow::Cow::Borrowed(bytes);
    };
    let body = &bytes[bom_len..];
    let body = &body[..body.len() - (body.len() % 2)];
    let (decoded, _) = encoding.decode_without_bom_handling(body);
    std::borrow::Cow::Owned(decoded.into_owned().into_bytes())
}

fn validate_worktree_bom(
    file: &Path,
    worktree: &WorktreeEncoding,
    prefix: &[u8],
    file_len: u64,
) -> Result<(), BoxError> {
    let actual = if prefix.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Some(&[0xEF, 0xBB, 0xBF][..])
    } else if prefix.starts_with(&[0xFF, 0xFE]) {
        Some(&[0xFF, 0xFE][..])
    } else if prefix.starts_with(&[0xFE, 0xFF]) {
        Some(&[0xFE, 0xFF][..])
    } else {
        None
    };
    let invalid = match worktree.bom {
        WorktreeBom::Unspecified if worktree.encoding == encoding_rs::UTF_8 => {
            actual.is_some_and(|bom| bom != [0xEF, 0xBB, 0xBF])
        }
        WorktreeBom::Unspecified => false,
        WorktreeBom::Required(expected) => file_len > 0 && actual != Some(expected),
        WorktreeBom::RequiredUtf16 => {
            file_len > 0 && actual != Some(&[0xFF, 0xFE][..]) && actual != Some(&[0xFE, 0xFF][..])
        }
        WorktreeBom::Forbidden => actual.is_some(),
    };
    if invalid {
        return Err(format!(
            "{}: contents do not satisfy working-tree-encoding={} BOM requirements",
            file.display(),
            worktree.label
        )
        .into());
    }
    Ok(())
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
/// canonical [`crate::encoding::parse_line_ending`]. Otherwise no explicit
/// override is returned: each mutating command resolves the complete Git policy
/// automatically. The remaining parameters are retained for API compatibility.
pub fn resolve_write_override(
    explicit: Option<&str>,
    _file: &Path,
    _git_root: Option<&Path>,
    _eol_normalize: bool,
) -> Result<Option<LineEnding>, BoxError> {
    if let Some(s) = explicit {
        return Ok(Some(crate::encoding::parse_line_ending(s)?));
    }
    Ok(None)
}

/// Canonicalise `file`, tolerating a not-yet-existing leaf (canonicalise the
/// parent and re-attach the file name) so write-time callers can resolve the
/// expectation for a path before it exists on disk.
fn canonicalize_lenient(file: &Path) -> Option<PathBuf> {
    let absolute = if file.is_absolute() {
        file.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(file)
    };
    if let Ok(canonical) = std::fs::canonicalize(&absolute) {
        return Some(canonical);
    }

    let mut existing = absolute.as_path();
    let mut suffix = Vec::new();
    while !existing.exists() {
        suffix.push(existing.file_name()?.to_owned());
        existing = existing.parent()?;
    }
    let mut rebuilt = std::fs::canonicalize(existing).ok()?;
    for component in suffix.iter().rev() {
        rebuilt.push(component);
    }
    Some(rebuilt)
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

/// Compare a buffer's actual line endings against the expectation carried by
/// an already-resolved [`FilePolicy`].
///
/// This is a **pure function**: it touches only its arguments, never
/// gitoxide, a filesystem path, or any cache. It is the function multi-file
/// callers should use once policy has been resolved up front (see
/// [`resolve_policies`]) — it lets per-file processing (including parallel
/// per-file workers) determine EOL mismatches without ever calling back into
/// this module's repository-discovery machinery.
///
/// `bytes` is the file's raw on-disk content (not yet decoded); worktree
/// encoding, if any, is applied internally via [`eol_analysis_bytes`] exactly
/// as [`GitEol::detect`] does.
///
/// Returns `None` when git would not normalise this path at all (no
/// `line_ending` in `policy`), when `text=auto` classified the content as
/// binary, or when the content already matches the expectation.
pub fn detect_with_policy(policy: &FilePolicy, bytes: &[u8]) -> Option<EolMismatch> {
    // No definite EOL policy: binary / `-text` / no applicable rule.
    let expected = policy.line_ending?;
    let analysis_bytes = eol_analysis_bytes(bytes, policy.working_tree_encoding.as_ref());
    let stats = Stats::from_bytes(&analysis_bytes);

    // In `auto` modes git leaves binary content untouched, so a binary buffer
    // is never a mismatch even though an EOL would otherwise be expected.
    if policy.auto_text && stats.is_binary() {
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
    //
    // `expected` is always `Lf` or `CrLf` here: it is produced exclusively by
    // `mode_to_line_ending`, which never yields `Cr` (git's own `Mode` enum
    // has no bare-CR variant).
    let actual = match expected {
        LineEnding::CrLf => dominant_of(&[
            (LineEnding::Lf, stats.lone_lf as u64),
            (LineEnding::Cr, stats.lone_cr as u64),
        ]),
        LineEnding::Lf => dominant_of(&[
            (LineEnding::CrLf, stats.crlf as u64),
            (LineEnding::Cr, stats.lone_cr as u64),
        ]),
        LineEnding::Cr => None,
    }?;

    Some(EolMismatch { expected, actual })
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

/// Automatic-discovery variant of [`emit_eol_advisory`].
pub fn emit_eol_advisory_auto(notes: &mut dyn std::io::Write, file: &Path) -> std::io::Result<()> {
    if let Some(line) = advisory_note_for_path(file) {
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

    #[test]
    fn automatic_policy_discovery_resolves_eol_and_encoding() {
        let dir = init_repo(
            Some("*.txt text eol=crlf working-tree-encoding=UTF-16LE-BOM\n"),
            "",
        );
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let file = nested.join("a.txt");

        let policy = policy_for_path(&file).unwrap();
        assert_eq!(policy.line_ending, Some(LineEnding::CrLf));
        let worktree = policy.working_tree_encoding.expect("worktree encoding");
        assert_eq!(worktree.encoding, encoding_rs::UTF_16LE);
        assert_eq!(worktree.bom, WorktreeBom::Required(&[0xFF, 0xFE]));
    }

    #[test]
    fn automatic_discovery_handles_multiple_repositories_below_one_root() {
        let parent = tempfile::tempdir().unwrap();
        let left = parent.path().join("left");
        let right = parent.path().join("right");
        fs::create_dir(&left).unwrap();
        fs::create_dir(&right).unwrap();
        gix::init(&left).unwrap();
        gix::init(&right).unwrap();
        fs::write(left.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
        fs::write(right.join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();

        assert_eq!(
            policy_for_path(&left.join("a.txt")).unwrap().line_ending,
            Some(LineEnding::Lf)
        );
        assert_eq!(
            policy_for_path(&right.join("a.txt")).unwrap().line_ending,
            Some(LineEnding::CrLf)
        );
    }

    #[test]
    fn automatic_discovery_prefers_nested_repository() {
        let outer = init_repo(Some("*.txt text eol=lf\n"), "");
        let nested = outer.path().join("nested");
        fs::create_dir(&nested).unwrap();
        gix::init(&nested).unwrap();
        fs::write(nested.join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();

        assert_eq!(
            policy_for_path(&outer.path().join("outer.txt"))
                .unwrap()
                .line_ending,
            Some(LineEnding::Lf)
        );
        assert_eq!(
            policy_for_path(&nested.join("inner.txt"))
                .unwrap()
                .line_ending,
            Some(LineEnding::CrLf)
        );
    }

    #[test]
    fn new_nested_repository_is_discovered_in_next_operation() {
        let outer = init_repo(Some("*.txt text eol=lf\n"), "");
        let nested = outer.path().join("nested");
        let child = nested.join("child");
        fs::create_dir_all(&child).unwrap();

        begin_operation();
        assert_eq!(
            policy_for_path(&child.join("before.txt"))
                .unwrap()
                .line_ending,
            Some(LineEnding::Lf)
        );

        gix::init(&nested).unwrap();
        fs::write(nested.join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();

        begin_operation();
        assert_eq!(
            policy_for_path(&child.join("after.txt"))
                .unwrap()
                .line_ending,
            Some(LineEnding::CrLf)
        );
    }

    #[test]
    fn begin_operation_clears_stale_no_repository_cache_entry() {
        // Unlike the nested-repository case above (where gix's own
        // attribute stack transparently picks up a nested `.gitattributes`
        // file even through a stale cached repository handle, masking
        // whether the cache was actually cleared), a directory that starts
        // with *no* repository at all caches a `None` root. If that
        // directory later becomes a repository itself, only clearing the
        // cache (via `begin_operation`) lets the next lookup see it --
        // there is no repository handle to transparently fall through to.
        let parent = tempfile::tempdir().unwrap();
        let standalone = parent.path().join("standalone");
        fs::create_dir_all(&standalone).unwrap();

        begin_operation();
        assert_eq!(
            policy_for_path(&standalone.join("before.txt"))
                .unwrap()
                .line_ending,
            None,
            "no repository exists yet"
        );

        gix::init(&standalone).unwrap();
        fs::write(standalone.join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();
        let cfg_path = standalone.join(".git").join("config");
        let mut cfg = fs::read_to_string(&cfg_path).unwrap_or_default();
        cfg.push_str("\n[core]\n\tautocrlf = false\n");
        fs::write(&cfg_path, cfg).unwrap();

        begin_operation();
        assert_eq!(
            policy_for_path(&standalone.join("after.txt"))
                .unwrap()
                .line_ending,
            Some(LineEnding::CrLf),
            "begin_operation must clear the stale 'no repository' cache entry \
             so the newly-created repository is discovered"
        );
    }

    #[test]
    fn text_auto_applies_eol_policy_only_to_text_content() {
        let dir = init_repo(Some("*.dat text=auto eol=crlf\n"), "");
        let policy = policy_for_path(&dir.path().join("a.dat")).unwrap();

        assert_eq!(
            policy.line_ending_for_text("alpha\nbeta\n"),
            Some(LineEnding::CrLf)
        );
        assert_eq!(policy.line_ending_for_text("alpha\0beta\n"), None);
    }

    #[test]
    fn latin_1_git_alias_is_rejected_instead_of_misdecoded() {
        let dir = init_repo(
            Some("*.txt working-tree-encoding=latin-1\n"),
            "\tautocrlf = false\n",
        );
        let error = policy_for_path(&dir.path().join("a.txt")).unwrap_err();
        assert!(error.to_string().contains("not supported losslessly"));
    }

    #[test]
    fn eol_detection_decodes_utf16_worktree_bytes() {
        let dir = init_repo(
            Some("*.txt text eol=lf working-tree-encoding=UTF-16LE-BOM\n"),
            "",
        );
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "a\r\nb\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let file = write_file(dir.path(), "a.txt", &bytes);

        let mismatch = detect_for_path(&file, &bytes)
            .unwrap()
            .expect("UTF-16 EOL mismatch");
        assert_eq!(mismatch.expected, LineEnding::Lf);
        assert_eq!(mismatch.actual, LineEnding::CrLf);
        assert!(advisory_note_for_path(&file).is_some());
    }

    #[test]
    fn utf8_worktree_encoding_rejects_utf16_bom() {
        let dir = init_repo(Some("*.txt working-tree-encoding=UTF-8\n"), "");
        let file = write_file(dir.path(), "a.txt", &[0xFF, 0xFE, b'a', 0]);

        let error = crate::read_text_file(&file, crate::IoMode::Buffered).unwrap_err();
        assert!(error.to_string().contains("BOM requirements"));
    }

    // ── `resolve_policies` / `detect_with_policy` (enumerate → resolve →
    // parallelize model) ────────────────────────────────────────────────────

    #[test]
    fn resolve_policies_resolves_each_path_across_multiple_repositories() {
        let parent = tempfile::tempdir().unwrap();
        let left = parent.path().join("left");
        let right = parent.path().join("right");
        fs::create_dir(&left).unwrap();
        fs::create_dir(&right).unwrap();
        gix::init(&left).unwrap();
        gix::init(&right).unwrap();
        fs::write(left.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
        fs::write(right.join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();

        let left_file = left.join("a.txt");
        let right_file = right.join("a.txt");
        let outside_file = parent.path().join("outside.txt");

        let resolved = resolve_policies(vec![
            left_file.clone(),
            right_file.clone(),
            outside_file.clone(),
        ]);

        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0].path, left_file);
        assert_eq!(
            resolved[0].policy.as_ref().unwrap().line_ending,
            Some(LineEnding::Lf)
        );
        assert_eq!(resolved[1].path, right_file);
        assert_eq!(
            resolved[1].policy.as_ref().unwrap().line_ending,
            Some(LineEnding::CrLf)
        );
        // Outside any repository: default policy (no Git line-ending
        // expectation), not an error.
        assert_eq!(resolved[2].path, outside_file);
        assert_eq!(resolved[2].policy.as_ref().unwrap().line_ending, None);
    }

    #[test]
    fn resolve_policies_reports_per_path_failure_without_aborting_batch() {
        let dir = init_repo(Some("*.txt working-tree-encoding=latin-1\n"), "");
        let bad = dir.path().join("a.txt");
        let good = dir.path().join("b.dat");

        let resolved = resolve_policies(vec![bad.clone(), good.clone()]);

        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].path, bad);
        assert!(resolved[0].policy.is_err());
        // The failing path does not prevent the next path from resolving.
        assert_eq!(resolved[1].path, good);
        assert!(resolved[1].policy.is_ok());
    }

    #[test]
    fn detect_with_policy_matches_git_eol_detect_for_crlf_expectation() {
        let dir = init_repo(Some("*.txt text eol=crlf\n"), "");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let file = write_file(dir.path(), "a.txt", b"x\r\ny\nz\r\n");
        let bytes = b"x\r\ny\nz\r\n";

        let via_gitoxide = git.detect(&file, bytes).unwrap();
        let policy = policy_for_path(&file).unwrap();
        let via_policy = detect_with_policy(&policy, bytes);

        assert_eq!(via_gitoxide, via_policy);
        assert_eq!(via_policy.unwrap().expected, LineEnding::CrLf);
        assert_eq!(via_policy.unwrap().actual, LineEnding::Lf);
    }

    #[test]
    fn detect_with_policy_is_pure_and_never_touches_gitoxide() {
        // A `FilePolicy` built entirely by hand (no repository involved at
        // all) must still classify mismatches correctly -- this is the
        // guarantee parallel workers depend on: `detect_with_policy` needs
        // nothing but the struct itself and a byte slice.
        let policy = FilePolicy {
            line_ending: Some(LineEnding::Lf),
            working_tree_encoding: None,
            ..FilePolicy::default()
        };
        let mismatch = detect_with_policy(&policy, b"a\r\nb\r\n").expect("CRLF vs expected LF");
        assert_eq!(mismatch.expected, LineEnding::Lf);
        assert_eq!(mismatch.actual, LineEnding::CrLf);

        assert_eq!(detect_with_policy(&policy, b"a\nb\n"), None, "already LF");
        assert_eq!(
            detect_with_policy(&FilePolicy::default(), b"a\r\nb\r\n"),
            None,
            "no definite line ending means git would not normalise"
        );
    }

    // ── line_ending_name ─────────────────────────────────────────────────────

    #[test]
    fn line_ending_name_covers_all_variants() {
        assert_eq!(line_ending_name(LineEnding::Lf), "LF");
        assert_eq!(line_ending_name(LineEnding::CrLf), "CRLF");
        assert_eq!(line_ending_name(LineEnding::Cr), "CR");
    }

    // ── FilePolicy::source_config ────────────────────────────────────────────

    #[test]
    fn source_config_with_no_worktree_encoding_only_sets_line_ending() {
        let policy = FilePolicy {
            line_ending: Some(LineEnding::CrLf),
            working_tree_encoding: None,
            ..FilePolicy::default()
        };
        let config = policy.source_config(Some(LineEnding::CrLf));
        assert_eq!(config.line_ending_default, Some(LineEnding::CrLf));
        assert_eq!(config.encoding_hint, None);
    }

    #[test]
    fn source_config_utf8_unspecified_bom_honours() {
        let policy = FilePolicy {
            working_tree_encoding: Some(WorktreeEncoding {
                label: "UTF-8".into(),
                encoding: encoding_rs::UTF_8,
                bom: WorktreeBom::Unspecified,
            }),
            ..FilePolicy::default()
        };
        let config = policy.source_config(None);
        assert_eq!(config.bom_policy, SourceBomPolicy::Honour);
        assert_eq!(config.encoding_hint, Some(encoding_rs::UTF_8));
    }

    #[test]
    fn source_config_non_utf8_unspecified_bom_ignores() {
        let policy = FilePolicy {
            working_tree_encoding: Some(WorktreeEncoding {
                label: "windows-1252".into(),
                encoding: encoding_rs::WINDOWS_1252,
                bom: WorktreeBom::Unspecified,
            }),
            ..FilePolicy::default()
        };
        let config = policy.source_config(None);
        assert_eq!(config.bom_policy, SourceBomPolicy::Ignore);
    }

    #[test]
    fn source_config_required_and_forbidden_bom_policies() {
        let required = FilePolicy {
            working_tree_encoding: Some(WorktreeEncoding {
                label: "UTF-16LE-BOM".into(),
                encoding: encoding_rs::UTF_16LE,
                bom: WorktreeBom::Required(&[0xFF, 0xFE]),
            }),
            ..FilePolicy::default()
        };
        assert_eq!(
            required.source_config(None).bom_policy,
            SourceBomPolicy::Honour
        );

        let required_utf16 = FilePolicy {
            working_tree_encoding: Some(WorktreeEncoding {
                label: "UTF-16".into(),
                encoding: encoding_rs::UTF_16LE,
                bom: WorktreeBom::RequiredUtf16,
            }),
            ..FilePolicy::default()
        };
        assert_eq!(
            required_utf16.source_config(None).bom_policy,
            SourceBomPolicy::Honour
        );

        let forbidden = FilePolicy {
            working_tree_encoding: Some(WorktreeEncoding {
                label: "UTF-16LE".into(),
                encoding: encoding_rs::UTF_16LE,
                bom: WorktreeBom::Forbidden,
            }),
            ..FilePolicy::default()
        };
        assert_eq!(
            forbidden.source_config(None).bom_policy,
            SourceBomPolicy::Ignore
        );
    }

    // ── FilePolicy::line_ending_for_worktree_bytes ──────────────────────────

    #[test]
    fn line_ending_for_worktree_bytes_skips_binary_under_text_auto() {
        let policy = FilePolicy {
            line_ending: Some(LineEnding::CrLf),
            working_tree_encoding: None,
            auto_text: true,
        };
        assert_eq!(
            policy.line_ending_for_worktree_bytes(b"alpha\nbeta\n"),
            Some(LineEnding::CrLf)
        );
        assert_eq!(
            policy.line_ending_for_worktree_bytes(b"alpha\0beta\n"),
            None,
            "text=auto must not normalise binary-looking content"
        );
    }

    // ── FilePolicy::validate_branch ──────────────────────────────────────────

    #[test]
    fn validate_branch_accepts_matching_bom_and_rejects_missing_bom() {
        let policy = FilePolicy {
            working_tree_encoding: Some(WorktreeEncoding {
                label: "UTF-8-BOM".into(),
                encoding: encoding_rs::UTF_8,
                bom: WorktreeBom::Required(&[0xEF, 0xBB, 0xBF]),
            }),
            ..FilePolicy::default()
        };
        let file = Path::new("a.txt");

        let with_bom = redwing::make_thicket_from_bytes(b"\xEF\xBB\xBFhello".to_vec()).main();
        assert!(policy.validate_branch(file, &*with_bom).is_ok());

        let without_bom = redwing::make_thicket_from_bytes(b"hello".to_vec()).main();
        let err = policy
            .validate_branch(file, &*without_bom)
            .expect_err("missing required BOM must be rejected");
        assert!(err.to_string().contains("BOM requirements"));
    }

    #[test]
    fn validate_branch_defers_bom_enforcement_for_empty_file() {
        // An empty file can't possibly carry a BOM yet; enforcement is
        // deferred until content is actually written (see
        // `validate_worktree_bom`'s `file_len > 0` guard).
        let policy = FilePolicy {
            working_tree_encoding: Some(WorktreeEncoding {
                label: "UTF-8-BOM".into(),
                encoding: encoding_rs::UTF_8,
                bom: WorktreeBom::Required(&[0xEF, 0xBB, 0xBF]),
            }),
            ..FilePolicy::default()
        };
        let empty = redwing::make_thicket_from_bytes(Vec::new()).main();
        assert!(policy.validate_branch(Path::new("a.txt"), &*empty).is_ok());
    }

    #[test]
    fn validate_branch_no_worktree_encoding_always_ok() {
        let policy = FilePolicy::default();
        let branch = redwing::make_thicket_from_bytes(b"anything at all".to_vec()).main();
        assert!(policy.validate_branch(Path::new("a.txt"), &*branch).is_ok());
    }

    // ── eol_analysis_bytes ───────────────────────────────────────────────────

    #[test]
    fn eol_analysis_bytes_decodes_utf16le_bom_with_even_body() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "ab".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let analysis = eol_analysis_bytes(&bytes, None);
        assert_eq!(&*analysis, b"ab");
    }

    #[test]
    fn eol_analysis_bytes_trims_trailing_odd_byte_in_utf16_body() {
        // An odd number of body bytes after the BOM (a truncated/corrupt
        // UTF-16 stream) must be trimmed to a whole number of code units
        // rather than passed to the decoder or panicking.
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "a".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.push(0x42); // stray trailing odd byte
        let analysis = eol_analysis_bytes(&bytes, None);
        assert_eq!(&*analysis, b"a");
    }

    #[test]
    fn eol_analysis_bytes_uses_worktree_encoding_when_no_bom_present() {
        // No BOM prefix, but the worktree declares UTF-16LE: the *declared*
        // encoding must still be applied.
        let worktree = WorktreeEncoding {
            label: "UTF-16LE".into(),
            encoding: encoding_rs::UTF_16LE,
            bom: WorktreeBom::Forbidden,
        };
        let mut bytes = Vec::new();
        for unit in "hi".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let analysis = eol_analysis_bytes(&bytes, Some(&worktree));
        assert_eq!(&*analysis, b"hi");
    }

    #[test]
    fn eol_analysis_bytes_passes_through_non_utf16_unchanged() {
        let analysis = eol_analysis_bytes(b"plain ascii\r\n", None);
        assert_eq!(&*analysis, b"plain ascii\r\n");
    }

    // ── validate_worktree_bom ────────────────────────────────────────────────

    #[test]
    fn validate_worktree_bom_utf8_unspecified_rejects_wrong_bom_only_when_present() {
        let worktree = WorktreeEncoding {
            label: "UTF-8".into(),
            encoding: encoding_rs::UTF_8,
            bom: WorktreeBom::Unspecified,
        };
        // No BOM at all: fine (UTF-8 BOM is optional).
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"hello", 5).is_ok());
        // Correct UTF-8 BOM: fine.
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"\xEF\xBB\xBFhi", 5).is_ok());
        // A *different* BOM (UTF-16LE) present under a declared UTF-8
        // worktree encoding is invalid.
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"\xFF\xFEhi", 4).is_err());
    }

    #[test]
    fn validate_worktree_bom_non_utf8_unspecified_never_rejects() {
        let worktree = WorktreeEncoding {
            label: "windows-1252".into(),
            encoding: encoding_rs::WINDOWS_1252,
            bom: WorktreeBom::Unspecified,
        };
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"\xFF\xFEhi", 4).is_ok());
    }

    #[test]
    fn validate_worktree_bom_required_utf16_accepts_either_byte_order() {
        let worktree = WorktreeEncoding {
            label: "UTF-16".into(),
            encoding: encoding_rs::UTF_16LE,
            bom: WorktreeBom::RequiredUtf16,
        };
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"\xFF\xFEhi", 4).is_ok());
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"\xFE\xFFhi", 4).is_ok());
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"hi", 2).is_err());
    }

    #[test]
    fn validate_worktree_bom_required_utf16_defers_for_empty_file() {
        let worktree = WorktreeEncoding {
            label: "UTF-16".into(),
            encoding: encoding_rs::UTF_16LE,
            bom: WorktreeBom::RequiredUtf16,
        };
        // `file_len == 0`: no BOM present, but enforcement is deferred.
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"", 0).is_ok());
    }

    #[test]
    fn validate_worktree_bom_forbidden_rejects_any_bom() {
        let worktree = WorktreeEncoding {
            label: "UTF-16LE".into(),
            encoding: encoding_rs::UTF_16LE,
            bom: WorktreeBom::Forbidden,
        };
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"hi", 2).is_ok());
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"\xFF\xFEhi", 4).is_err());
    }

    #[test]
    fn validate_worktree_bom_required_exact_rejects_other_boms() {
        let worktree = WorktreeEncoding {
            label: "UTF-8-BOM".into(),
            encoding: encoding_rs::UTF_8,
            bom: WorktreeBom::Required(&[0xEF, 0xBB, 0xBF]),
        };
        // Present but wrong BOM (UTF-16LE instead of UTF-8).
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"\xFF\xFEhi", 4).is_err());
        // No BOM at all on a non-empty file.
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"hi", 2).is_err());
        // Correct BOM.
        assert!(validate_worktree_bom(Path::new("a"), &worktree, b"\xEF\xBB\xBFhi", 5).is_ok());
    }

    // ── advisory_note / advisory_note_for_path (exact content) ──────────────

    #[test]
    fn advisory_note_reports_exact_expected_and_actual() {
        let dir = init_repo(Some("*.txt text eol=crlf\n"), "");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let file = write_file(dir.path(), "a.txt", b"alpha\nbeta\n");

        let note = git.advisory_note(&file).expect("mismatch note");
        assert_eq!(
            note,
            format!(
                "note: {}: line endings (LF) differ from git's expected CRLF \
                 (per .gitattributes / core.autocrlf / core.eol); run 'tpu doctor' to normalize",
                file.display()
            )
        );

        // Same content via the automatic-discovery free function.
        let note_auto = advisory_note_for_path(&file).expect("mismatch note");
        assert_eq!(note, note_auto);
    }

    #[test]
    fn advisory_note_is_none_when_endings_already_match() {
        let dir = init_repo(Some("*.txt text eol=lf\n"), "");
        let git = GitEol::open(dir.path()).unwrap().unwrap();
        let file = write_file(dir.path(), "a.txt", b"alpha\nbeta\n");
        assert_eq!(git.advisory_note(&file), None);
        assert_eq!(advisory_note_for_path(&file), None);
    }

    // ── emit_eol_advisory / emit_eol_advisory_auto (exact content) ──────────

    #[test]
    fn emit_eol_advisory_writes_exact_note_line() {
        let dir = init_repo(Some("*.txt text eol=crlf\n"), "");
        let file = write_file(dir.path(), "a.txt", b"alpha\nbeta\n");
        let mut buf = Vec::new();
        emit_eol_advisory(&mut buf, dir.path(), &file).unwrap();
        let expected = format!(
            "note: {}: line endings (LF) differ from git's expected CRLF \
             (per .gitattributes / core.autocrlf / core.eol); run 'tpu doctor' to normalize\n",
            file.display()
        );
        assert_eq!(String::from_utf8(buf).unwrap(), expected);
    }

    #[test]
    fn emit_eol_advisory_writes_nothing_on_match() {
        let dir = init_repo(Some("*.txt text eol=lf\n"), "");
        let file = write_file(dir.path(), "a.txt", b"alpha\nbeta\n");
        let mut buf = Vec::new();
        emit_eol_advisory(&mut buf, dir.path(), &file).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn emit_eol_advisory_auto_writes_exact_note_line() {
        let dir = init_repo(Some("*.txt text eol=crlf\n"), "");
        let file = write_file(dir.path(), "a.txt", b"alpha\nbeta\n");
        let mut buf = Vec::new();
        emit_eol_advisory_auto(&mut buf, &file).unwrap();
        let expected = format!(
            "note: {}: line endings (LF) differ from git's expected CRLF \
             (per .gitattributes / core.autocrlf / core.eol); run 'tpu doctor' to normalize\n",
            file.display()
        );
        assert_eq!(String::from_utf8(buf).unwrap(), expected);
    }

    #[test]
    fn emit_eol_advisory_auto_writes_nothing_on_match() {
        let dir = init_repo(Some("*.txt text eol=lf\n"), "");
        let file = write_file(dir.path(), "a.txt", b"alpha\nbeta\n");
        let mut buf = Vec::new();
        emit_eol_advisory_auto(&mut buf, &file).unwrap();
        assert!(buf.is_empty());
    }

    // ── parse_worktree_encoding: remaining documented spellings ─────────────

    #[test]
    fn worktree_encoding_bare_utf16_requires_either_bom() {
        let dir = init_repo(Some("*.txt working-tree-encoding=UTF-16\n"), "");
        let policy = policy_for_path(&dir.path().join("a.txt")).unwrap();
        let worktree = policy.working_tree_encoding.expect("worktree encoding");
        assert_eq!(worktree.encoding, encoding_rs::UTF_16LE);
        assert_eq!(worktree.bom, WorktreeBom::RequiredUtf16);
    }

    #[test]
    fn worktree_encoding_utf16be_bom_requires_be_bom() {
        let dir = init_repo(Some("*.txt working-tree-encoding=UTF-16BE-BOM\n"), "");
        let policy = policy_for_path(&dir.path().join("a.txt")).unwrap();
        let worktree = policy.working_tree_encoding.expect("worktree encoding");
        assert_eq!(worktree.encoding, encoding_rs::UTF_16BE);
        assert_eq!(worktree.bom, WorktreeBom::Required(&[0xFE, 0xFF]));
    }

    #[test]
    fn worktree_encoding_utf16le_bomless_forbids_bom() {
        let dir = init_repo(Some("*.txt working-tree-encoding=UTF-16LE\n"), "");
        let policy = policy_for_path(&dir.path().join("a.txt")).unwrap();
        let worktree = policy.working_tree_encoding.expect("worktree encoding");
        assert_eq!(worktree.encoding, encoding_rs::UTF_16LE);
        assert_eq!(worktree.bom, WorktreeBom::Forbidden);
    }

    #[test]
    fn worktree_encoding_utf16be_bomless_forbids_bom() {
        let dir = init_repo(Some("*.txt working-tree-encoding=UTF-16BE\n"), "");
        let policy = policy_for_path(&dir.path().join("a.txt")).unwrap();
        let worktree = policy.working_tree_encoding.expect("worktree encoding");
        assert_eq!(worktree.encoding, encoding_rs::UTF_16BE);
        assert_eq!(worktree.bom, WorktreeBom::Forbidden);
    }

    #[test]
    fn worktree_encoding_utf8_bom_requires_utf8_bom() {
        let dir = init_repo(Some("*.txt working-tree-encoding=UTF-8-BOM\n"), "");
        let policy = policy_for_path(&dir.path().join("a.txt")).unwrap();
        let worktree = policy.working_tree_encoding.expect("worktree encoding");
        assert_eq!(worktree.encoding, encoding_rs::UTF_8);
        assert_eq!(worktree.bom, WorktreeBom::Required(&[0xEF, 0xBB, 0xBF]));
    }

    // ── fold_digest via text=auto + eol=lf (complements the eol=crlf test) ──

    #[test]
    fn text_auto_with_eol_lf_still_applies_eol_policy_to_text_content() {
        let dir = init_repo(Some("*.dat text=auto eol=lf\n"), "");
        let policy = policy_for_path(&dir.path().join("a.dat")).unwrap();

        assert_eq!(
            policy.line_ending_for_text("alpha\r\nbeta\r\n"),
            Some(LineEnding::Lf)
        );
        assert_eq!(
            policy.line_ending_for_text("alpha\0beta\r\n"),
            None,
            "text=auto must still skip binary-looking content under eol=lf"
        );
    }
}
