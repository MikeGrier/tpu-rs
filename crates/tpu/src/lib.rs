// Copyright (c) 2026, Michael Grier

pub mod cmd;
pub mod data_format;
pub mod encoding;
pub mod escape;
pub mod message;
pub mod mojibake;
pub mod output;
pub mod rsp;
pub mod shell;
pub mod test_fixtures;

use std::{error::Error, fs, io, path::Path, sync::Arc, time::Duration};

use memmap2::MmapOptions;
use redwing::Branch;

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
    match mode {
        IoMode::Mmap => {
            let f = retry_io(|| fs::File::open(path))?;
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

/// Read raw file bytes, respecting the I/O mode.
///
/// In `Mmap` mode the file is memory-mapped and the relevant bytes are
/// copied out.  In `Buffered` mode `std::fs::read` is used directly.
///
/// Use this for commands that need raw `&[u8]` access without going through
/// a harrier `Source` (e.g. binary-mode head/tail, binary validators).
pub fn read_raw_bytes(path: &Path, mode: IoMode) -> io::Result<Vec<u8>> {
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
