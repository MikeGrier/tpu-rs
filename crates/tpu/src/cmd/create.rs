// Copyright (c) 2026, Michael Grier

//! `tpu create` — write text content to a **new** file, failing if the path
//! already exists.
//!
//! `create` exists so that callers with a "make a brand-new file" intent have
//! a tool whose name and contract match that intent exactly. The primary
//! consumer is the MCP `tpu_create_file` tool, where a distinct create
//! operation removes the ambiguity agents hit when trying to anticipate
//! whether `write` will create or overwrite.
//!
//! Unlike [`crate::cmd::write::run`], which this module used to delegate to,
//! `create` does not rely on a `Path::exists()`/`try_exists()` pre-check for
//! its no-clobber guarantee — a check like that is inherently racy, since
//! another process could create the file between the check and the write.
//! Instead, the actual persist step goes through [`crate::atomic_create_new`],
//! which uses `NamedTempFile::persist_noclobber` — an OS-level primitive that
//! atomically fails if the destination already exists. An initial
//! `try_exists()` check is still performed first purely to produce a fast,
//! friendly error message in the common case; it is advisory only.
//!
//! ## Contract
//!
//! - If `file` already exists at the time of the atomic create, [`run`]
//!   returns an error and nothing is written.  (A stranded `<file>.bak` from
//!   an interrupted prior write is first recovered, so a half-completed write
//!   still counts as "exists".)
//! - Otherwise the parent directories are created as needed and the content is
//!   written fresh.  New files default to UTF-8 with LF line endings; the
//!   `output_encoding`, `bom_policy`, and `line_ending_override` parameters
//!   override those defaults exactly as they do for [`crate::cmd::write::run`].
//! - The write-time mojibake guard applies (see [`crate::cmd::write::run`]);
//!   pass [`WritePolicy::permissive`] to disable it.
//!
//! Because the target is always a new file there is no `.bak` created and no
//! diff to emit — the whole content is the change.

use std::path::Path;

use harrier::encoding::LineEnding;

use crate::{
    IoMode,
    encoding::{BomPolicy, OutputEncoding},
    mojibake::WritePolicy,
};

/// Run the `create` subcommand: write `content` to `file`, refusing to
/// clobber an existing file.
///
/// Returns an error if `file` already exists (after recovering any stranded
/// `<file>.bak`).  The no-clobber guarantee is enforced atomically by
/// [`crate::atomic_create_new`] at persist time, not by the earlier
/// `try_exists()` check (which exists only to produce a fast, friendly error
/// message and is not itself race-free).  On success the file is created and
/// populated using the same encoding- and line-ending-aware defaults, and the
/// same write-time mojibake guard, used by [`crate::cmd::write::run`].
pub fn run(
    file: &Path,
    content: &str,
    output_encoding: OutputEncoding,
    bom_policy: BomPolicy,
    line_ending_override: Option<LineEnding>,
    io_mode: IoMode,
    policy: WritePolicy,
) -> Result<(), Box<dyn std::error::Error>> {
    // `create` never reads an existing file, so `io_mode` (which only
    // governs mmap-vs-buffered *reads*) has nothing to act on here. It is
    // kept as a parameter for signature parity with the other `cmd::*`
    // entry points and forward-compatibility.
    let _ = io_mode;

    // Promote a stranded backup first so an interrupted prior write is not
    // mistaken for an absent file (and then silently clobbered).
    let _ = crate::recover_stranded_backup(file);

    // Fast, advisory existence check purely for a clear, early error
    // message. `try_exists()` (rather than `exists()`) ensures a genuine
    // I/O error (permissions, a broken symlink, ...) is surfaced as an
    // error instead of being silently treated as "file does not exist".
    //
    // This check is *not* what makes the create-only guarantee safe: another
    // process could still create `file` between this check and the write
    // below. The actual no-clobber guarantee comes from
    // `atomic_create_new`'s OS-level `persist_noclobber` call, which is the
    // sole, atomic authority on whether `file` exists at persist time.
    match file.try_exists() {
        Ok(true) => {
            return Err(format!(
                "create: {}: file already exists (use write to overwrite)",
                file.display()
            )
            .into());
        }
        Ok(false) => {}
        Err(e) => return Err(format!("create: {}: {e}", file.display()).into()),
    }

    if policy.reject_introduced_mojibake {
        crate::mojibake::check_write_does_not_introduce_mojibake("", content)
            .map_err(|e| format!("create: {}: {e}", file.display()))?;
    }

    let bytes = crate::cmd::write::encode_new_file_content(
        content,
        output_encoding,
        bom_policy,
        line_ending_override,
    );

    crate::atomic_create_new(file, &bytes).map_err(|e| -> Box<dyn std::error::Error> {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "create: {}: file already exists (use write to overwrite)",
                file.display()
            )
            .into()
        } else {
            format!("create: {}: {e}", file.display()).into()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn td() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn creates_new_file_utf8_lf() {
        let dir = td();
        let path = dir.path().join("new.txt");
        run(
            &path,
            "hello\nworld\n",
            OutputEncoding::Preserve,
            BomPolicy::default(),
            None,
            IoMode::Buffered,
            WritePolicy::default(),
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello\nworld\n");
    }

    #[test]
    fn creates_parent_directories() {
        let dir = td();
        let path = dir.path().join("a").join("b").join("new.txt");
        run(
            &path,
            "x\n",
            OutputEncoding::Preserve,
            BomPolicy::default(),
            None,
            IoMode::Buffered,
            WritePolicy::default(),
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x\n");
    }

    #[test]
    fn refuses_to_overwrite_existing_file() {
        let dir = td();
        let path = dir.path().join("exists.txt");
        std::fs::write(&path, b"original").unwrap();
        let err = run(
            &path,
            "replacement",
            OutputEncoding::Preserve,
            BomPolicy::default(),
            None,
            IoMode::Buffered,
            WritePolicy::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
        // Original content must be untouched.
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
    }

    #[test]
    fn honours_crlf_line_ending_override() {
        let dir = td();
        let path = dir.path().join("crlf.txt");
        run(
            &path,
            "a\nb\n",
            OutputEncoding::Preserve,
            BomPolicy::default(),
            Some(LineEnding::CrLf),
            IoMode::Buffered,
            WritePolicy::default(),
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"a\r\nb\r\n");
    }

    #[test]
    fn recovered_stranded_backup_counts_as_existing() {
        let dir = td();
        let path = dir.path().join("stranded.txt");
        // Simulate an interrupted write: only <file>.bak is present.
        let bak = dir.path().join("stranded.txt.bak");
        std::fs::write(&bak, b"prior contents").unwrap();
        let err = run(
            &path,
            "new",
            OutputEncoding::Preserve,
            BomPolicy::default(),
            None,
            IoMode::Buffered,
            WritePolicy::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
        // The recovered file retains the prior contents.
        assert_eq!(std::fs::read(&path).unwrap(), b"prior contents");
    }
}
