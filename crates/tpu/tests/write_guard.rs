// Copyright (c) 2026, Michael Grier

//! Integration test for Milestone 2 — write-time mojibake guard.
//!
//! Five end-to-end scenarios prove the keystone:
//!
//! 1. `cmd::replace::run` on a clean UTF-8 file with a replacement
//!    that *would* inject the Latin-1 mojibake fingerprint is rejected;
//!    file untouched on disk.
//! 2. Same scenario with [`WritePolicy::permissive`] succeeds and the
//!    file is updated.
//! 3. `cmd::write::run` overwriting a file that already contained the
//!    em-dash mojibake fingerprint with identical bytes succeeds (no
//!    *new* corruption).
//! 4. `cmd::edit::run` splice that *removes* a region of mojibake
//!    succeeds.
//! 5. `cmd::append::run` adding clean content to a file with
//!    pre-existing mojibake succeeds (we don't punish writers for
//!    damage they didn't cause).
//!
//! All literal mojibake byte sequences come from
//! [`tpu::test_fixtures`] (decoded from base-64 at runtime) so this
//! source file remains pure ASCII and needs no `allow-mojibake` opt-out.

use std::{fs, io::Write, path::Path};

use tempfile::NamedTempFile;
use tpu::mojibake::{ALLOW_MARKER, WritePolicy};
use tpu::test_fixtures::{cafe, cafe_line, e_grave, em_dash};

/// Write `bytes` to a fresh temp file (deleted on drop) and return both
/// the handle (kept alive) and its path.
fn fixture(bytes: &[u8]) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("create tempfile");
    f.write_all(bytes).expect("write fixture");
    f.flush().expect("flush fixture");
    f
}

/// Read a file as raw bytes.
fn read(p: &Path) -> Vec<u8> {
    fs::read(p).expect("read file")
}

// ── (1) replace rejects newly-introduced mojibake ───────────────────────────

#[test]
fn replace_rejects_replacement_that_introduces_mojibake() {
    let file = fixture(b"hello world\n");
    let path = file.path().to_path_buf();
    let original = read(&path);

    // Replacement bytes are the canonical Latin-1 mojibake fingerprint
    // (decoded from base-64 in `tpu::test_fixtures`).
    let replacement_string = cafe();
    let replacement_bytes = replacement_string.as_bytes();

    let err = tpu::cmd::replace::run(
        &path,
        "world",
        replacement_bytes,
        false, // multiline
        true,  // fixed_strings
        None,  // line_ending_override
        None,  // diff_out
        false, // count_only
        false, // dry_run
        tpu::IoMode::Buffered,
        WritePolicy::default(),
    )
    .expect_err("replace must reject newly-introduced mojibake");

    let msg = err.to_string();
    assert!(
        msg.contains("introduce mojibake"),
        "error message must explain the rejection: {msg}"
    );
    assert!(
        msg.contains("--allow-mojibake"),
        "error message must point at the override flag: {msg}"
    );

    // File is unchanged on disk.
    assert_eq!(
        read(&path),
        original,
        "replace was rejected but the file was modified anyway"
    );
}

// ── (2) replace with --allow-mojibake succeeds ──────────────────────────────

#[test]
fn replace_with_allow_mojibake_flag_succeeds() {
    let file = fixture(b"hello world\n");
    let path = file.path().to_path_buf();

    let replacement_string = cafe();
    let replacement_bytes = replacement_string.as_bytes();

    let n = tpu::cmd::replace::run(
        &path,
        "world",
        replacement_bytes,
        false,
        true,
        None,
        None,
        false,
        false,
        tpu::IoMode::Buffered,
        WritePolicy::permissive(),
    )
    .expect("replace must succeed when guard is permissive");

    assert_eq!(n, 1, "exactly one substitution expected");

    // File should now contain "hello " + the cafe mojibake bytes + "\n".
    let new_bytes = read(&path);
    let mut expected = b"hello ".to_vec();
    expected.extend_from_slice(replacement_bytes);
    expected.push(b'\n');
    assert_eq!(
        new_bytes, expected,
        "file should now contain the mojibake bytes verbatim"
    );
}

// ── (3) write same-corrupt over same-corrupt is allowed ─────────────────────

#[test]
fn write_same_preexisting_corruption_is_allowed() {
    // File already contains the punctuation-mojibake fingerprint of a
    // real em-dash.  Re-writing the identical text must not be
    // punished: the writer didn't cause the damage.
    let original_text = format!("intro {} end\n", em_dash());
    let file = fixture(original_text.as_bytes());
    let path = file.path().to_path_buf();

    tpu::cmd::write::run(
        &path,
        &original_text,
        tpu::encoding::OutputEncoding::Preserve,
        tpu::encoding::BomPolicy::default(),
        None,
        None,
        tpu::IoMode::Buffered,
        WritePolicy::default(),
    )
    .expect("rewriting identical bytes must succeed under default policy");

    // After write the original is renamed to `<file>.bak`; new content
    // is at the original path.  Verify both content and bak-file
    // existence (since this exercises the atomic-write code path).
    assert_eq!(read(&path), original_text.as_bytes());
    let bak = format!("{}.bak", path.display());
    assert!(Path::new(&bak).exists(), "expected .bak file");
    let _ = fs::remove_file(&bak);
}

// ── (4) edit splice that removes mojibake is allowed ────────────────────────

#[test]
fn edit_splice_that_removes_mojibake_is_allowed() {
    // Text mode operates on lines.  Two lines: a clean greeting and a
    // mojibake'd line we want to delete.
    let original = format!("clean line\n{} corrupt\n", cafe());
    let file = fixture(original.as_bytes());
    let path = file.path().to_path_buf();

    // Delete line 2 (1-based).  We use the helper to convert.
    let ops = vec![tpu::cmd::edit::EditOp::Splice {
        // line_range_to_source_bytes is private; the public API takes
        // source byte offsets.  Compute manually for this fixture:
        // "clean line\n" is 11 bytes, end of line 2 is the EOF.
        start: 11,
        end: original.len(),
        data: Vec::new(),
    }];

    let n = tpu::cmd::edit::run(
        &path,
        ops,
        true, // binary mode: feed source byte offsets directly
        None,
        None,
        tpu::IoMode::Buffered,
        WritePolicy::default(),
    )
    .expect("edit removing mojibake must succeed");

    assert_eq!(n, 1);
    assert_eq!(read(&path), b"clean line\n");
    let _ = fs::remove_file(format!("{}.bak", path.display()));
}

// ── (5) append clean content to pre-existing mojibake is allowed ────────────

#[test]
fn append_clean_content_to_corrupt_file_is_allowed() {
    // File already has Latin1-mojibake.  Appending clean text adds
    // zero new mojibake matches → write proceeds.
    let original = format!("preexisting {}", cafe_line());
    let file = fixture(original.as_bytes());
    let path = file.path().to_path_buf();

    let new_content = "more clean text\n";
    tpu::cmd::append::run(
        &path,
        new_content,
        None,
        None,
        tpu::IoMode::Buffered,
        WritePolicy::default(),
    )
    .expect("appending clean text must succeed even when the old file is corrupt");

    let combined = read(&path);
    let expected = {
        let mut v = Vec::new();
        v.extend_from_slice(original.as_bytes());
        v.extend_from_slice(new_content.as_bytes());
        v
    };
    assert_eq!(combined, expected);
}

// ── (6) extra: write that introduces NEW mojibake to a corrupt file ─────────
//
// Bonus assertion beyond the five-scenario theme: the per-pattern
// budget logic must reject an *additional* mojibake match even when
// the file already has some.  This locks down the "set difference"
// semantics from M2-2's contract.

#[test]
fn write_that_adds_new_mojibake_to_corrupt_file_is_rejected() {
    // Old has one Latin1 mojibake.  New content introduces a *second*
    // (the `e`-grave fingerprint) — must be rejected.
    let original = format!("alpha {}\n", cafe());
    let new_text = format!("alpha {} and {}\n", cafe(), e_grave());

    let file = fixture(original.as_bytes());
    let path = file.path().to_path_buf();

    let err = tpu::cmd::write::run(
        &path,
        &new_text,
        tpu::encoding::OutputEncoding::Preserve,
        tpu::encoding::BomPolicy::default(),
        None,
        None,
        tpu::IoMode::Buffered,
        WritePolicy::default(),
    )
    .expect_err("must reject the second introduced match");

    assert!(err.to_string().contains("introduce mojibake"));
    // File must be unmodified (atomic write semantics).
    assert_eq!(read(&path), original.as_bytes());
}

// ── (7) extra: brand-new file with allow-marker is allowed ──────────────────

#[test]
fn write_new_file_with_allow_marker_succeeds_even_with_mojibake() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("brand_new.txt");
    assert!(!path.exists());

    let content = format!(
        "// {}\nthis fixture intentionally has {} in it\n",
        ALLOW_MARKER,
        cafe()
    );

    tpu::cmd::write::run(
        &path,
        &content,
        tpu::encoding::OutputEncoding::Preserve,
        tpu::encoding::BomPolicy::default(),
        None,
        None,
        tpu::IoMode::Buffered,
        WritePolicy::default(),
    )
    .expect("allow-marker in new content must permit mojibake");

    assert_eq!(read(&path), content.as_bytes());
}
