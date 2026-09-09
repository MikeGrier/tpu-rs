// Copyright (c) 2026, Michael Grier

pub mod cmd;
pub mod data_format;
pub mod encoding;
pub mod escape;
pub mod git;
pub mod message;
pub mod mojibake;
pub mod output;
pub mod rsp;
pub mod shell;
pub mod test_fixtures;
pub mod walk;

use std::{
    error::Error,
    fs, io,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use memmap2::MmapOptions;
use redwing::Branch;
use tempfile::NamedTempFile;

/// Controls how tpu reads file content into a redwing branch.
///
/// `Mmap` (default) memory-maps the file for demand-paged access.
/// `Buffered` reads the entire file into memory via `std::fs::read`.
///
/// The MCP server uses `Buffered` because Windows Defender can terminate
/// LLVM-built processes that create memory-mapped file regions in rapid
/// succession.  The CLI uses `Mmap` for normal interactive use where the
/// rate of operations is low enough not to trigger Defender heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IoMode {
    /// Memory-map the file (default for CLI).
    #[default]
    Mmap,
    /// Read the entire file into a heap buffer (used by the MCP server).
    Buffered,
}

/// Open a file and return a redwing `Branch` suitable for harrier/encoding
/// processing.
///
/// In `Mmap` mode the file is memory-mapped for demand-paged access.
/// In `Buffered` mode the file is read entirely into memory.
///
/// The returned branch is the `.main()` handle of a fresh `Thicket`.
pub fn open_as_branch(path: &Path, mode: IoMode) -> Result<Arc<dyn Branch>, Box<dyn Error>> {
    let _ = recover_stranded_backup(path);
    match mode {
        IoMode::Mmap => {
            let f = retry_io(|| fs::File::open(path))?;
            if f.metadata()?.len() == 0 {
                return Ok(redwing::make_thicket_from_bytes(Vec::new()).main());
            }
            // SAFETY: bytes are accessed read-only through the Branch API.
            let mmap = unsafe { MmapOptions::new().map(&f) }?;
            drop(f);
            Ok(redwing::make_thicket_from_mmap(mmap).main())
        }

        IoMode::Buffered => {
            let bytes = retry_io(|| fs::read(path))?;
            Ok(redwing::make_thicket_from_bytes(bytes).main())
        }
    }
}

/// Open a text source using Git's worktree policy when `path` belongs to a
/// repository.
///
/// The nearest repository is discovered through [`git::policy_for_path`].
/// A resolved `working-tree-encoding` becomes harrier's authoritative encoding
/// hint, while a definite Git EOL policy becomes its line-ending default.
pub fn open_source(path: &Path, mode: IoMode) -> Result<harrier::source::Source, Box<dyn Error>> {
    let branch = open_as_branch(path, mode)?;
    source_from_branch(path, branch)
}

/// Build a Git-policy-aware text source from an already-open branch.
///
/// Resolves policy via [`git::policy_for_path`] on every call. Multi-file
/// pipelines that have already resolved policy up front (see
/// [`git::resolve_policies`]) should call
/// [`source_from_branch_with_policy`] instead, so this does not repeat a
/// gitoxide call already made during that pipeline's sequential enrichment
/// phase.
pub fn source_from_branch(
    path: &Path,
    branch: Arc<dyn Branch>,
) -> Result<harrier::source::Source, Box<dyn Error>> {
    let policy = git::policy_for_path(path)?;
    source_from_branch_with_policy(path, branch, &policy)
}

/// As [`source_from_branch`], but takes an already-resolved [`git::FilePolicy`]
/// instead of resolving one internally. Never calls gitoxide.
pub fn source_from_branch_with_policy(
    path: &Path,
    branch: Arc<dyn Branch>,
    policy: &git::FilePolicy,
) -> Result<harrier::source::Source, Box<dyn Error>> {
    policy.validate_branch(path, &*branch)?;
    let raw = redwing::materialize(&*branch)?;
    let line_ending = policy.line_ending_for_worktree_bytes(&raw);
    Ok(harrier::source::Source::new(
        branch,
        policy.source_config(line_ending),
    )?)
}

/// Fully decoded, LF-normalised representation of a text file.
#[derive(Debug)]
pub struct DecodedTextFile {
    pub text: String,
    pub encoding: &'static encoding_rs::Encoding,
    pub line_ending: harrier::encoding::LineEnding,
    pub bom_len: usize,
    pub layout: TextLayout,
}

/// Logical line and terminator metadata captured before LF normalisation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextLayout {
    pub line_count: u64,
    pub has_lf: bool,
    pub has_crlf: bool,
    pub has_cr: bool,
}

impl TextLayout {
    fn analyze(text: &str) -> Self {
        let bytes = text.as_bytes();
        let mut layout = Self::default();
        // Iterator-driven walk (no manual index counter): the position is
        // advanced solely by the shared `iter.next()` calls below, so there is
        // no `i += 1`-style step to mutate into an infinite loop. The one
        // extra `iter.next()` in the CRLF arm consumes the paired `\n` (the
        // same two bytes the old `i += 2` used to skip).
        let mut iter = bytes.iter().enumerate();
        while let Some((idx, &b)) = iter.next() {
            match b {
                b'\r' if bytes.get(idx + 1) == Some(&b'\n') => {
                    layout.has_crlf = true;
                    layout.line_count += 1;
                    iter.next();
                }
                b'\r' => {
                    layout.has_cr = true;
                    layout.line_count += 1;
                }
                b'\n' => {
                    layout.has_lf = true;
                    layout.line_count += 1;
                }
                _ => {}
            }
        }
        if !bytes.is_empty() && !matches!(bytes.last(), Some(b'\r' | b'\n')) {
            layout.line_count += 1;
        }
        layout
    }
}

#[cfg(test)]
mod text_layout_tests {
    use super::*;

    #[test]
    fn analyze_empty_string_yields_default_layout() {
        let layout = TextLayout::analyze("");
        assert_eq!(layout, TextLayout::default());
    }

    #[test]
    fn analyze_lone_lf() {
        let layout = TextLayout::analyze("\n");
        assert!(layout.has_lf);
        assert!(!layout.has_cr);
        assert!(!layout.has_crlf);
        assert_eq!(layout.line_count, 1);
    }

    #[test]
    fn analyze_lone_cr() {
        let layout = TextLayout::analyze("\r");
        assert!(!layout.has_lf);
        assert!(layout.has_cr);
        assert!(!layout.has_crlf);
        assert_eq!(layout.line_count, 1);
    }

    #[test]
    fn analyze_crlf_pair() {
        let layout = TextLayout::analyze("\r\n");
        assert!(!layout.has_lf);
        assert!(!layout.has_cr);
        assert!(layout.has_crlf);
        assert_eq!(layout.line_count, 1);
    }

    /// A `\r` not immediately followed by `\n` must be classified as a lone
    /// CR, never as CRLF -- this pins the lookahead guard
    /// (`bytes.get(idx + 1) == Some(&b'\n')`) against a mutation that makes
    /// it unconditionally true (which would misclassify every `\r` as CRLF
    /// and incorrectly consume the following byte as if it were `\n`).
    #[test]
    fn analyze_cr_not_followed_by_lf_is_lone_cr_not_crlf() {
        let layout = TextLayout::analyze("\ra");
        assert!(layout.has_cr);
        assert!(!layout.has_crlf);
        // "\r" (line 1) + trailing unterminated "a" (line 2).
        assert_eq!(layout.line_count, 2);
    }

    /// A file whose last byte is a terminator must not get an extra phantom
    /// line counted for content after it (there is none) -- pins the
    /// trailing-line guard against a mutation that drops its `!bytes.is_empty()`
    /// half (which would otherwise only fire when the buffer is empty).
    #[test]
    fn analyze_trailing_newline_does_not_add_a_phantom_line() {
        let layout = TextLayout::analyze("a\n");
        assert_eq!(layout.line_count, 1);
    }

    /// A file with content after the last terminator gets one extra line
    /// counted for that trailing, unterminated content.
    #[test]
    fn analyze_content_after_last_terminator_adds_one_line() {
        let layout = TextLayout::analyze("a\nb");
        assert_eq!(layout.line_count, 2);
    }

    /// Exercises every terminator kind in one buffer and pins the exact
    /// line_count arithmetic (each terminator, plus one for the trailing
    /// unterminated "d").
    #[test]
    fn analyze_mixed_line_endings_counts_each_line_and_sets_every_flag() {
        let layout = TextLayout::analyze("a\nb\r\nc\rd");
        assert!(layout.has_lf);
        assert!(layout.has_crlf);
        assert!(layout.has_cr);
        assert_eq!(layout.line_count, 4);
    }
}

/// Decode a complete text file according to Git/harrier policy and normalise
/// CRLF and lone CR terminators to LF in Unicode space.
///
/// Unicode-space normalisation is required for UTF-16: byte-oriented line
/// scanners cannot distinguish its interleaved CR/LF code-unit bytes safely.
pub fn read_text_file(path: &Path, mode: IoMode) -> Result<DecodedTextFile, Box<dyn Error>> {
    let branch = open_as_branch(path, mode)?;
    let source = source_from_branch(path, Arc::clone(&branch))?;
    let encoding = source.encoding();
    let line_ending = source.line_ending();
    let bom_len = source.bom_len();
    let raw = redwing::materialize(&*branch)?;
    let body = &raw[bom_len.min(raw.len())..];
    let (decoded, had_errors) = encoding.decode_without_bom_handling(body);
    if had_errors {
        return Err(format!("{} is not valid {}", path.display(), encoding.name()).into());
    }
    let layout = TextLayout::analyze(&decoded);
    let text = decoded.replace("\r\n", "\n").replace('\r', "\n");
    Ok(DecodedTextFile {
        text,
        encoding,
        line_ending,
        bom_len,
        layout,
    })
}

/// Read raw file bytes, respecting the I/O mode.
///
/// In `Mmap` mode the file is memory-mapped and the relevant bytes are
/// copied out.  In `Buffered` mode `std::fs::read` is used directly.
///
/// Use this for commands that need raw `&[u8]` access without going through
/// a harrier `Source` (e.g. binary-mode head/tail, binary validators).
pub fn read_raw_bytes(path: &Path, mode: IoMode) -> io::Result<Vec<u8>> {
    let _ = recover_stranded_backup(path);
    match mode {
        IoMode::Mmap => {
            let f = retry_io(|| fs::File::open(path))?;
            // SAFETY: bytes are accessed read-only and copied out immediately.
            let mmap = unsafe { MmapOptions::new().map(&f) }?;
            drop(f);
            Ok(mmap[..].to_vec())
        }
        IoMode::Buffered => retry_io(|| fs::read(path)),
    }
}

/// Compute a content digest over a file's current visible bytes and return it
/// as a lowercase hex string (16 chars, XXH3-64).
///
/// The digest is produced by streaming the file through a redwing branch (no
/// full materialization) and depends only on the byte content — two files
/// with identical bytes yield the same token regardless of encoding or how
/// they were produced.  It is the strong compare-and-swap ("version") token
/// used to detect that a file changed between an agent's read and its write:
/// see the MCP layer's `if_match` precondition.
pub fn content_digest(path: &Path, mode: IoMode) -> Result<String, Box<dyn Error>> {
    let branch = open_as_branch(path, mode)?;
    let d = redwing::digest(
        &*branch,
        redwing::DigestAlgorithm::Xxh3_64,
        redwing::Canonicalization::Identity,
    )?;
    Ok(d.to_hex())
}

// ── Cross-process write lock ──────────────────────────────────────────────────

/// Suffix for the per-file advisory-lock sidecar.
const LOCK_SUFFIX: &str = ".tpulock";

/// Longest wall-clock time [`acquire_write_lock`] will wait for a contended
/// lock before falling back to unlocked (best-effort) behavior.  A single
/// write holds the lock for milliseconds, so reaching this bound means a peer
/// is wedged; proceeding unlocked preserves availability rather than hanging
/// the caller indefinitely.
const LOCK_WAIT_CAP: Duration = Duration::from_secs(5);

/// An exclusive, cross-process advisory lock serialising every `tpu` writer to
/// one file.  Held for the duration of a single finalization (version re-check
/// → temp→`.bak`→persist swap → `.bak` cleanup) and released on drop.
///
/// The lock is anchored on a **stable hidden sidecar** `<file>.tpulock`, never
/// on the data file itself: the atomic-write swap renames the data file to
/// `<file>.bak` and installs a fresh inode at the original path, so a lock
/// taken on the data file would ride the rename onto the now-`.bak` inode and
/// stop excluding a later writer that opens the (new-inode) path.  The sidecar
/// is never renamed, so its lock identity is stable across the swap.
pub struct WriteLock {
    file: fs::File,
    #[allow(dead_code)] // retained for diagnostics / future use
    path: PathBuf,
}

impl Drop for WriteLock {
    fn drop(&mut self) {
        // Explicit unlock is belt-and-suspenders: the OS also releases the
        // advisory lock when the handle closes (and, on Windows, the
        // FILE_FLAG_DELETE_ON_CLOSE handle removes the sidecar as the last
        // holder closes).
        let _ = self.file.unlock();
    }
}

/// Build the hidden sidecar lock path for `file`, or `None` when the resulting
/// file-name component would be too long for the filesystem — in which case
/// the caller falls back to today's unlocked behavior.
///
/// Hidden means the Windows hidden attribute (set at open, see
/// [`open_lock_file`]) plus, on Unix, a leading-dot name.
fn lock_sidecar_path(file: &Path) -> Option<PathBuf> {
    let name = file.file_name()?;
    let mut lock_name = std::ffi::OsString::new();
    #[cfg(not(windows))]
    lock_name.push("."); // Unix "hidden" convention
    lock_name.push(name);
    lock_name.push(LOCK_SUFFIX);
    // Cheap guard against a doomed syscall: 255 is the near-universal maximum
    // file-name component length. `OsString::len()` is the platform's encoded
    // length (UTF-8 bytes on Unix, UTF-16 code units on Windows), not a
    // grapheme count — good enough for this bound. The authoritative per-volume
    // limit is enforced by the OS when we try to open the sidecar (a
    // name-too-long error also falls back — see `acquire_write_lock`).
    if lock_name.len() > 255 {
        return None;
    }
    Some(file.with_file_name(lock_name))
}

#[cfg(windows)]
fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // Minimal Win32 constants (avoid a windows-sys import churn for four values).
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const DELETE: u32 = 0x0001_0000;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_SHARE_DELETE: u32 = 0x4;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
    retry_io(|| {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            // DELETE access is required for FILE_FLAG_DELETE_ON_CLOSE.
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
            // FILE_SHARE_DELETE lets other holders open the same sidecar while
            // it is marked delete-on-close, so it survives (ref-counted) until
            // the LAST holder releases — no waiter is ever orphaned.
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .attributes(FILE_ATTRIBUTE_HIDDEN)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .create(true)
            .open(path)
    })
}

#[cfg(not(windows))]
fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    // The Unix sidecar is a stable, dot-hidden anchor left on disk (deleting it
    // would reintroduce the inode-swap race the sidecar exists to avoid).
    retry_io(|| {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
    })
}

/// Acquire the exclusive cross-process write lock for `file`.
///
/// **Best-effort by design** — locking is a hardening layer that must never
/// make a write fail or hang:
/// - `Some(guard)` — the lock is held until `guard` drops.
/// - `None` — locking was skipped and the caller proceeds exactly as before
///   (unlocked): the sidecar name would exceed the volume's max path segment,
///   the sidecar could not be opened, or a wedged peer held the lock past
///   [`LOCK_WAIT_CAP`].
pub fn acquire_write_lock(file: &Path) -> Option<WriteLock> {
    let lock_path = lock_sidecar_path(file)?;
    let f = open_lock_file(&lock_path).ok()?;
    let start = Instant::now();
    loop {
        match f.try_lock() {
            Ok(()) => {
                return Some(WriteLock {
                    file: f,
                    path: lock_path,
                });
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                if start.elapsed() >= LOCK_WAIT_CAP {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(std::fs::TryLockError::Error(_)) => return None,
        }
    }
}

/// Retry a closure that returns [`io::Result<T>`] up to 5 retries on transient
/// Windows AV/Defender errors (sharing violation or access denied), sleeping
/// 25 ms between attempts (6 total attempts).  Returns immediately on any
/// other error or after exhausting all retries.
///
/// The minimum practical Windows sleep quantum is ~15 ms; 25 ms is chosen to
/// comfortably clear a single AV scan window.  Five retries add at most 125 ms,
/// which is imperceptible in practice and far less disruptive than the operation
/// failing outright.
pub fn retry_io<T, F: FnMut() -> io::Result<T>>(mut f: F) -> io::Result<T> {
    const MAX_RETRIES: u32 = 5;
    const RETRY_DELAY: Duration = Duration::from_millis(25);
    let mut attempt = 0u32;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if attempt < MAX_RETRIES && is_transient_io_error(&e) => {
                attempt += 1;
                std::thread::sleep(RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Returns `true` for transient Windows I/O errors that may clear with a
/// short retry delay (AV/Defender scan in progress: sharing violation = os
/// error 32, access denied = os error 5).  Always `false` on non-Windows
/// platforms where these codes do not have the same transient meaning.
fn is_transient_io_error(e: &io::Error) -> bool {
    #[cfg(windows)]
    {
        // Named constants for the two Windows error codes that can appear
        // transiently while AV/Defender is scanning a file.
        const ERROR_ACCESS_DENIED: i32 = 5;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        matches!(
            e.raw_os_error(),
            Some(ERROR_ACCESS_DENIED) | Some(ERROR_SHARING_VIOLATION)
        )
    }
    #[cfg(not(windows))]
    {
        let _ = e;
        false
    }
}

// ── Stranded-backup recovery ──────────────────────────────────────────────────

#[cfg(test)]
mod retry_io_tests {
    use super::*;
    use std::sync::mpsc;

    /// Run `f` on a background thread and wait up to `timeout` for it to
    /// finish, returning its result.
    ///
    /// `retry_io`'s termination guard (`attempt < MAX_RETRIES &&
    /// is_transient_io_error(&e)`) has mutants — the guard replaced with an
    /// unconditional `true`, `&&` replaced with `||`, and the `attempt += 1`
    /// counter replaced with a no-op — that turn a persistently-failing
    /// closure into a genuine infinite loop (real 25ms sleeps between
    /// iterations, never terminating). Observing "did retry_io correctly
    /// give up" requires a closure that keeps failing, so there is no way to
    /// design a test for this that both (a) proves termination and (b) is
    /// guaranteed to itself terminate quickly if that termination is broken.
    /// Bounding the wait in a background thread converts "hang until cargo
    /// mutants' own ~112-220s per-mutant timeout" into a fast, deterministic
    /// test failure (a few hundred ms). The background thread is
    /// deliberately leaked if `f` never returns; `libtest` force-exits the
    /// process after the suite finishes, so a leaked spinning thread does
    /// not block the test binary from exiting.
    fn run_with_timeout<T: Send + 'static>(
        timeout: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(timeout).unwrap_or_else(|_| {
            panic!(
                "operation did not complete within {timeout:?} -- retry_io's \
                 termination guard appears to be broken (infinite retry loop)"
            )
        })
    }

    #[test]
    fn retry_io_succeeds_immediately_without_retrying() {
        let mut calls = 0;
        let result = retry_io(|| {
            calls += 1;
            Ok::<_, io::Error>(42)
        });
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls, 1, "a first-try success must not retry");
    }

    #[test]
    fn retry_io_propagates_non_transient_errors_immediately() {
        // Bounded via `run_with_timeout`: a closure that always fails is the
        // only way to observe "does retry_io ever stop retrying", and that
        // same closure would hang forever under the guard-replaced-with-
        // `true` mutant (which retries regardless of transience). This is
        // cross-platform (unlike the transient-error tests below) because
        // that specific mutation doesn't depend on `is_transient_io_error`
        // at all -- it can hang on any platform given any persistently
        // failing closure.
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_inner = Arc::clone(&calls);
        let result: io::Result<()> = run_with_timeout(Duration::from_secs(2), move || {
            retry_io(move || {
                calls_inner.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(io::Error::from(io::ErrorKind::NotFound))
            })
        });
        assert!(result.is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a non-transient error must not be retried"
        );
    }

    #[cfg(windows)]
    #[test]
    fn retry_io_gives_up_after_exactly_max_retries_on_transient_error() {
        // ERROR_SHARING_VIOLATION: recognised as transient by
        // `is_transient_io_error` on Windows. Bounded via `run_with_timeout`
        // (see its doc comment): this closure always fails, which is the
        // only way to pin the exact give-up count, and is exactly the shape
        // that hangs under the guard/`&&`/attempt-counter mutants.
        const ERROR_SHARING_VIOLATION: i32 = 32;
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls_inner = Arc::clone(&calls);
        let result: io::Result<()> = run_with_timeout(Duration::from_secs(2), move || {
            retry_io(move || {
                calls_inner.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(io::Error::from_raw_os_error(ERROR_SHARING_VIOLATION))
            })
        });
        assert!(result.is_err());
        // MAX_RETRIES (5) retries after the initial attempt = 6 total calls.
        // This pins the exact retry-count boundary: an off-by-one in the
        // `attempt < MAX_RETRIES` comparison changes this count.
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 6);
    }

    #[cfg(windows)]
    #[test]
    fn retry_io_succeeds_once_transient_error_clears_within_budget() {
        const ERROR_ACCESS_DENIED: i32 = 5;
        let mut calls = 0;
        let result = retry_io(|| {
            calls += 1;
            if calls < 3 {
                Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED))
            } else {
                Ok(99)
            }
        });
        assert_eq!(result.unwrap(), 99);
        assert_eq!(calls, 3);
    }
}

/// Return the `<file>.bak` companion path used by the atomic-write swap in
/// `cmd::write`, `cmd::replace`, `cmd::append`, and `cmd::edit`.
fn backup_path_for(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// If `file` is missing but `<file>.bak` exists, rename the `.bak` back to
/// the original path.  Returns `Ok(true)` when a recovery happened,
/// `Ok(false)` when nothing needed doing.
///
/// ## Why
///
/// The atomic-write swap (tempfile in same dir → rename original to
/// `<file>.bak` → persist temp into `<file>`) has a small crash window
/// between the two renames.  If the process is terminated there — Windows
/// Defender killing the worker, power loss, etc. — the file ends up at
/// `<file>.bak` with nothing at the original path.  Without recovery, every
/// subsequent `append`/`replace`/`edit`/read against the original path
/// fails with "file does not exist", even though the prior contents are
/// sitting one directory entry away.
///
/// This helper is called automatically by [`open_as_branch`] and
/// [`read_raw_bytes`], and is also called explicitly at the top of the
/// mutating `cmd::write`, `cmd::append`, and `cmd::edit` entry points so
/// that recovery happens before their own `file.exists()` pre-flight
/// checks decide a fresh-create vs. update path.  `cmd::replace` does not
/// need a separate call: it reaches the file through [`open_as_branch`],
/// which already performs recovery.
///
/// All errors are non-fatal — recovery is best-effort.  If the rename
/// fails (sharing violation, permission denied, …) the caller will see
/// the original "file does not exist" error on the next operation, which
/// is the same behaviour as before this helper existed.  Likewise, if
/// `try_exists` itself fails (permission/ACL errors), recovery is
/// skipped so we never rename based on an indeterminate existence
/// check.
pub fn recover_stranded_backup(file: &Path) -> io::Result<bool> {
    // Cheap exit: file present, nothing to recover.  If existence cannot
    // be determined (e.g. permission/ACL error), skip recovery rather
    // than risk an unintended rename.
    match file.try_exists() {
        Ok(true) => return Ok(false),
        Ok(false) => {}
        Err(_) => return Ok(false),
    }
    let bak = backup_path_for(file);
    match bak.try_exists() {
        Ok(true) => {}
        Ok(false) => return Ok(false),
        Err(_) => return Ok(false),
    }
    // Recover.  Use retry_io because the .bak may still be held briefly by
    // an AV scan triggered by the very crash that stranded it.
    retry_io(|| fs::rename(&bak, file))?;
    Ok(true)
}

// ── Shared atomic write ───────────────────────────────────────────────────────

/// Atomically replace (or create) `file` with `bytes`.
///
/// This is the single implementation of the temp→`.bak`→persist→restore swap
/// shared by every mutating `cmd::*` command (`write`, `append`, `replace`,
/// `edit`, and the binary `write` path).  Extracting it guarantees the
/// resilience behaviour is uniform and prevents the variants from drifting
/// apart.
///
/// Behaviour:
/// - A temp file is created in `file`'s parent directory (same filesystem, so
///   the final swap is a rename rather than a copy) and `bytes` written to it.
/// - If `file` already exists it is first renamed to `<file>.bak`; the temp
///   file is then persisted into place.  If the persist fails, the `.bak` is
///   renamed back so the original content is never lost.
/// - If `file` does not exist, parent directories are created first and the
///   temp file is persisted directly.
///
/// The directory-creation, rename, and persist steps are each wrapped in
/// [`retry_io`] so a transient Windows AV/Defender sharing violation does not
/// fail the write.  The temp-file `write_all`/`flush` are not retried — a
/// partially written temp file cannot be safely re-driven without extra
/// bookkeeping, and a failure there simply discards the (not-yet-installed)
/// temp file, leaving the original untouched.
///
/// Callers remain responsible for any pre-write content checks (e.g. the
/// mojibake guard) and post-write side effects (e.g. diff emission); this
/// helper only performs the byte-for-byte atomic swap.
pub fn atomic_write(file: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let file_exists = file.try_exists()?;
    if !file_exists {
        retry_io(|| fs::create_dir_all(dir))?;
    }

    let mut tmp = retry_io(|| NamedTempFile::new_in(dir))?;
    tmp.write_all(bytes)?;
    tmp.flush()?;

    if file_exists {
        let bak = backup_path_for(file);
        retry_io(|| fs::rename(file, &bak))?;
        persist_with_retry(tmp, file).inspect_err(|_e| {
            let _ = retry_io(|| fs::rename(&bak, file)); // best-effort restore
        })
    } else {
        persist_with_retry(tmp, file)
    }
}

/// Persist `tmp` to `dest`, retrying on transient Windows AV errors.
///
/// On a transient failure the temp file (returned inside the `PersistError`)
/// is recovered and the persist re-attempted, so retries do not leak the
/// handle.
fn persist_with_retry(tmp: NamedTempFile, dest: &Path) -> io::Result<()> {
    let mut tmp = Some(tmp);
    retry_io(|| {
        let t = tmp
            .take()
            .expect("persist retry: temp file already consumed");
        match t.persist(dest) {
            Ok(_) => Ok(()),
            Err(e) => {
                tmp = Some(e.file);
                Err(e.error)
            }
        }
    })
}

/// Persist `tmp` to `dest` only if `dest` does not already exist, retrying on
/// transient Windows AV errors.
///
/// A non-transient failure (in particular `io::ErrorKind::AlreadyExists`,
/// which `NamedTempFile::persist_noclobber` returns when `dest` is present)
/// is returned immediately without retrying.
fn persist_noclobber_with_retry(tmp: NamedTempFile, dest: &Path) -> io::Result<()> {
    let mut tmp = Some(tmp);
    retry_io(|| {
        let t = tmp
            .take()
            .expect("persist retry: temp file already consumed");
        match t.persist_noclobber(dest) {
            Ok(_) => Ok(()),
            Err(e) => {
                tmp = Some(e.file);
                Err(e.error)
            }
        }
    })
}

/// Atomically create `file` with `bytes`, failing with
/// [`io::ErrorKind::AlreadyExists`] if it already exists.
///
/// This is the create-only counterpart to [`atomic_write`], used by
/// `cmd::create::run`.  It performs no separate existence pre-check of its
/// own: the temp file is persisted into place via
/// `NamedTempFile::persist_noclobber`, an OS-level no-clobber primitive
/// (`link`+`unlink` on Unix, a non-replacing `MoveFileEx` on Windows) that is
/// the sole, atomic authority on whether `file` exists at persist time. This
/// closes the TOCTOU window that a `Path::exists()`/`try_exists()` check
/// followed by a plain rename-into-place would leave open — another process
/// could otherwise create `file` in between the check and the write, and
/// the plain rename would silently clobber it.
///
/// Callers that want a fast, advisory "does this already exist" check for an
/// early, friendly error message may still perform one before calling this
/// function; it just must not be relied upon for correctness, since this
/// function re-validates atomically regardless.
pub fn atomic_create_new(file: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    retry_io(|| fs::create_dir_all(dir))?;

    let mut tmp = retry_io(|| NamedTempFile::new_in(dir))?;
    tmp.write_all(bytes)?;
    tmp.flush()?;

    persist_noclobber_with_retry(tmp, file)
}

#[cfg(test)]
mod atomic_create_new_tests {
    use super::*;

    #[test]
    fn creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("new.txt");
        atomic_create_new(&f, b"hello").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"hello");
    }

    #[test]
    fn creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a").join("b").join("new.txt");
        atomic_create_new(&f, b"hello").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"hello");
    }

    #[test]
    fn refuses_to_clobber_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("exists.txt");
        fs::write(&f, b"original").unwrap();
        let err = atomic_create_new(&f, b"replacement").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        // The existing file must be completely untouched by the failed attempt.
        assert_eq!(fs::read(&f).unwrap(), b"original");
    }

    /// Regression test for the TOCTOU race this function exists to close: a
    /// file created *after* any earlier existence check but *before* the
    /// final persist must still be refused, because the no-clobber guarantee
    /// comes from the persist call itself, not from a separate check.
    #[test]
    fn refuses_to_clobber_file_created_after_call_starts_conceptually() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("race.txt");
        // Simulate "another process" winning the race by creating the file
        // immediately before the no-clobber persist call, with no
        // intervening existence check on our side.
        fs::write(&f, b"winner").unwrap();
        let err = atomic_create_new(&f, b"loser").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&f).unwrap(), b"winner");
    }
}

#[cfg(test)]
mod content_digest_tests {
    use super::*;

    /// Two files with identical bytes must produce the same content-version
    /// token (the digest is over content, not path or construction), and the
    /// token is the fixed-width 16-char XXH3-64 hex string.
    #[test]
    fn identical_content_same_digest() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        fs::write(&a, b"hello world\n").unwrap();
        fs::write(&b, b"hello world\n").unwrap();
        let da = content_digest(&a, IoMode::Buffered).unwrap();
        let db = content_digest(&b, IoMode::Buffered).unwrap();
        assert_eq!(da, db, "identical bytes must yield identical digests");
        assert_eq!(
            da.len(),
            16,
            "xxh3-64 hex token must be 16 chars; got {da:?}"
        );
    }

    /// A single-byte change must change the token — this is what makes it a
    /// usable compare-and-swap version.
    #[test]
    fn one_byte_change_changes_digest() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        fs::write(&f, b"hello world\n").unwrap();
        let before = content_digest(&f, IoMode::Buffered).unwrap();
        fs::write(&f, b"hello worlt\n").unwrap();
        let after = content_digest(&f, IoMode::Buffered).unwrap();
        assert_ne!(before, after, "a one-byte change must change the digest");
    }

    /// An empty file has a well-defined, stable 16-char token (not an error).
    #[test]
    fn empty_file_has_stable_token() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("empty.txt");
        fs::write(&f, b"").unwrap();
        let a = content_digest(&f, IoMode::Buffered).unwrap();
        let b = content_digest(&f, IoMode::Buffered).unwrap();
        assert_eq!(a, b, "the same empty file must digest identically");
        assert_eq!(a.len(), 16);
    }

    /// The digest is over raw bytes, so the Mmap and Buffered read paths must
    /// agree for the same file (the token must not depend on how it was read).
    #[test]
    fn mmap_and_buffered_agree() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        fs::write(&f, b"some bytes\nacross lines\n").unwrap();
        let m = content_digest(&f, IoMode::Mmap).unwrap();
        let b = content_digest(&f, IoMode::Buffered).unwrap();
        assert_eq!(m, b, "Mmap and Buffered must yield the same token");
    }

    /// Digesting a nonexistent file surfaces an error rather than a bogus
    /// token — the MCP layer relies on this to distinguish "absent" from a
    /// real content token.
    #[test]
    fn missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("nope.txt");
        assert!(content_digest(&f, IoMode::Buffered).is_err());
    }
}

#[cfg(test)]
mod write_lock_tests {
    use super::*;

    /// While one holder has the write lock, an independent handle to the same
    /// sidecar must be unable to take the exclusive lock — this is the actual
    /// cross-process mutual exclusion (exercised here in-process via two
    /// separate open file descriptions, which contend the same way).
    #[test]
    fn write_lock_excludes_a_second_holder() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("data.txt");
        fs::write(&f, b"x").unwrap();

        let g = acquire_write_lock(&f).expect("first acquisition must succeed");

        let sidecar = lock_sidecar_path(&f).unwrap();
        let other = open_lock_file(&sidecar).unwrap();
        assert!(
            matches!(other.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "a second holder must be blocked while the first holds the lock"
        );

        drop(g);
        assert!(
            other.try_lock().is_ok(),
            "the lock must be acquirable again after the first holder releases"
        );
        let _ = other.unlock();
    }

    /// A file name whose sidecar would exceed the max path segment falls back
    /// to unlocked (best-effort) behavior rather than erroring the write.
    #[test]
    fn overlong_sidecar_name_falls_back_to_unlocked() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a".repeat(300));
        assert!(
            lock_sidecar_path(&f).is_none(),
            "an overlong name must yield no sidecar path"
        );
        assert!(
            acquire_write_lock(&f).is_none(),
            "acquire must fall back to None (proceed unlocked)"
        );
    }

    /// Acquire → release → re-acquire must work: releasing frees the lock and
    /// leaves no state that blocks the next writer (no self-deadlock, no stale
    /// lock).
    #[test]
    fn sequential_acquire_release_reacquire() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("seq.txt");
        fs::write(&f, b"x").unwrap();
        let g1 = acquire_write_lock(&f).expect("first acquisition");
        drop(g1);
        let g2 = acquire_write_lock(&f).expect("re-acquire after release");
        drop(g2);
    }

    /// Pins the `lock_name.len() > 255` boundary exactly: a 255-char sidecar
    /// name must still succeed, while 256 must fall back to `None`. Computed
    /// relative to `LOCK_SUFFIX` and the platform-specific hidden-dot prefix
    /// so the test is exact on both Windows (`OpenOptionsExt`-based) and Unix
    /// builds, rather than assuming one platform's constant offset.
    #[test]
    fn lock_sidecar_path_exact_255_char_boundary() {
        let prefix_len: usize = if cfg!(windows) { 0 } else { 1 };
        let target_total = 255usize;
        let name_len = target_total - prefix_len - LOCK_SUFFIX.len();

        let dir = std::path::PathBuf::from("dir");
        let at_255 = dir.join("a".repeat(name_len));
        let sidecar = lock_sidecar_path(&at_255).expect("exactly 255 chars must still succeed");
        assert_eq!(sidecar.file_name().unwrap().len(), 255);

        let at_256 = dir.join("a".repeat(name_len + 1));
        assert!(
            lock_sidecar_path(&at_256).is_none(),
            "256 chars must be rejected"
        );
    }

    /// The sidecar is opened with `FILE_FLAG_DELETE_ON_CLOSE`.
    ///
    /// (Empirically, this Windows version honours delete-on-close even when
    /// `DELETE` is absent from the requested access mask, so this test alone
    /// does not pin that specific bit -- see
    /// `open_lock_file_grants_read_write_and_delete_access` for the test that
    /// does, by probing each bit's real capability directly.)
    #[cfg(windows)]
    #[test]
    fn open_lock_file_sidecar_is_deleted_once_last_handle_closes() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("d.txt");
        fs::write(&f, b"x").unwrap();
        let sidecar = lock_sidecar_path(&f).unwrap();
        {
            let _h = open_lock_file(&sidecar).unwrap();
            assert!(
                sidecar.exists(),
                "sidecar should exist while a handle is open"
            );
        }
        assert!(
            !sidecar.exists(),
            "sidecar must be auto-deleted once the last handle closes"
        );
    }

    /// Probes each bit of `open_lock_file`'s requested access mask
    /// (`GENERIC_READ | GENERIC_WRITE | DELETE`) via the handle's *actual*
    /// read/write capability, rather than via a side effect (locking,
    /// delete-on-close) that this Windows version turns out to still honour
    /// even when a bit is missing from the mask. `&` has higher precedence
    /// than `|` in Rust, so an operator-swap mutation on this expression can
    /// silently drop an operand from a *different* side of the `|` than the
    /// mutated operator's own position (e.g. `GENERIC_WRITE & DELETE`
    /// evaluates to `0`, so `GENERIC_READ | (GENERIC_WRITE & DELETE)` reduces
    /// to plain `GENERIC_READ`) -- reading and writing the handle directly
    /// distinguishes this precisely regardless of which side collapsed.
    #[cfg(windows)]
    #[test]
    fn open_lock_file_grants_read_write_and_delete_access() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("rw.txt");
        fs::write(&f, b"x").unwrap();
        let sidecar = lock_sidecar_path(&f).unwrap();
        let mut h = open_lock_file(&sidecar).unwrap();

        use std::io::{Read, Seek, SeekFrom, Write};
        h.write_all(b"probe").expect("handle must be writable");
        h.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = String::new();
        h.read_to_string(&mut buf).expect("handle must be readable");
        assert_eq!(buf, "probe");
    }

    /// A contended lock must be *waited out* (retried), not given up on
    /// instantly -- pins `start.elapsed() >= LOCK_WAIT_CAP` against a `<`
    /// mutation, which would return `None` on the very first contention
    /// check instead of retrying until the holder releases (or the real
    /// multi-second cap elapses).
    #[test]
    fn acquire_write_lock_waits_for_contended_lock_then_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("contend.txt");
        fs::write(&f, b"x").unwrap();

        let holder = acquire_write_lock(&f).expect("first acquisition must succeed");
        let f2 = f.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(holder);
            let _ = f2; // keep path alive for clarity; not otherwise used here
        });

        let start = Instant::now();
        let second = acquire_write_lock(&f);
        let elapsed = start.elapsed();
        handle.join().unwrap();

        assert!(
            second.is_some(),
            "must eventually acquire once the first lock releases"
        );
        assert!(
            elapsed >= Duration::from_millis(100),
            "must have retried/waited for the contended lock rather than giving up \
             immediately (elapsed={elapsed:?})"
        );
    }
}

#[cfg(test)]
mod recover_tests {
    use super::*;

    #[test]
    fn recover_no_file_no_bak_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("missing.txt");
        let recovered = recover_stranded_backup(&f).unwrap();
        assert!(!recovered);
        assert!(!f.exists());
    }

    #[test]
    fn recover_file_present_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("present.txt");
        fs::write(&f, b"hi").unwrap();
        let bak = backup_path_for(&f);
        fs::write(&bak, b"stale").unwrap();
        let recovered = recover_stranded_backup(&f).unwrap();
        assert!(!recovered, "must not touch an extant file");
        assert_eq!(fs::read(&f).unwrap(), b"hi");
        assert_eq!(fs::read(&bak).unwrap(), b"stale");
    }

    #[test]
    fn recover_promotes_bak_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("stranded.txt");
        let bak = backup_path_for(&f);
        fs::write(&bak, b"recovered content").unwrap();
        assert!(!f.exists());
        let recovered = recover_stranded_backup(&f).unwrap();
        assert!(recovered);
        assert_eq!(fs::read(&f).unwrap(), b"recovered content");
        assert!(!bak.exists(), ".bak should be consumed by recovery");
    }
}

#[cfg(test)]
mod git_text_policy_tests {
    use super::*;

    #[test]
    fn read_text_file_uses_discovered_worktree_encoding() {
        let repo = tempfile::tempdir().unwrap();
        gix::init(repo.path()).unwrap();
        fs::write(
            repo.path().join(".gitattributes"),
            "*.txt text eol=crlf working-tree-encoding=UTF-16LE-BOM\n",
        )
        .unwrap();
        let file = repo.path().join("a.txt");
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "alpha\r\n\u{03b2}\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(&file, bytes).unwrap();

        let decoded = read_text_file(&file, IoMode::Buffered).unwrap();
        assert_eq!(decoded.text, "alpha\n\u{03b2}\n");
        assert_eq!(decoded.encoding, encoding_rs::UTF_16LE);
        assert_eq!(decoded.line_ending, harrier::encoding::LineEnding::CrLf);
        assert_eq!(decoded.bom_len, 2);
        assert_eq!(decoded.layout.line_count, 2);
        assert!(decoded.layout.has_crlf);
        assert!(!decoded.layout.has_lf);
    }

    #[test]
    fn read_text_file_rejects_missing_required_worktree_bom() {
        let repo = tempfile::tempdir().unwrap();
        gix::init(repo.path()).unwrap();
        fs::write(
            repo.path().join(".gitattributes"),
            "*.txt working-tree-encoding=UTF-16LE-BOM\n",
        )
        .unwrap();
        let file = repo.path().join("a.txt");
        fs::write(&file, [b'a', 0, b'\n', 0]).unwrap();

        let error = read_text_file(&file, IoMode::Buffered).unwrap_err();
        assert!(error.to_string().contains("BOM requirements"));
    }

    #[test]
    fn read_text_file_treats_worktree_encoding_as_authoritative() {
        let repo = tempfile::tempdir().unwrap();
        gix::init(repo.path()).unwrap();
        fs::write(
            repo.path().join(".gitattributes"),
            "*.txt working-tree-encoding=windows-1252\n",
        )
        .unwrap();
        let file = repo.path().join("ascii.txt");
        fs::write(&file, b"plain ascii\n").unwrap();

        let decoded = read_text_file(&file, IoMode::Buffered).unwrap();
        assert_eq!(decoded.encoding, encoding_rs::WINDOWS_1252);
        assert_eq!(decoded.text, "plain ascii\n");
    }

    #[test]
    fn read_text_file_strictly_validates_declared_encoding() {
        let repo = tempfile::tempdir().unwrap();
        gix::init(repo.path()).unwrap();
        fs::write(
            repo.path().join(".gitattributes"),
            "*.txt working-tree-encoding=UTF-8\n",
        )
        .unwrap();
        let file = repo.path().join("invalid.txt");
        fs::write(&file, [0xFF, b'\n']).unwrap();

        let error = read_text_file(&file, IoMode::Buffered).unwrap_err();
        assert!(error.to_string().contains("not valid UTF-8"));
    }
}
