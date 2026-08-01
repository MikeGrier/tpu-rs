// Copyright (c) 2026, Michael Grier

//! `tpu write` — write text content (UTF-8/LF) to a file, re-encoding and
//! denormalising line endings to match the target file's conventions.
//!
//! ## Write-time mojibake guard
//!
//! Before any bytes touch disk, [`run`] forwards the *would-be* file
//! content through
//! [`crate::mojibake::check_write_does_not_introduce_mojibake`].  When
//! the proposed bytes contain mojibake patterns — Latin-1, punctuation,
//! box-drawing, NBSP, or double-encoded fingerprints — that were *not*
//! present in the file's prior content, the write is rejected with
//! [`crate::mojibake::MojibakeIntroduced`] and the file is left untouched.  Existing damage is preserved
//! without complaint — only newly-introduced matches trigger a
//! refusal.  This is the same guard that protects `replace`, `edit`,
//! and `append`.
//!
//! Pass [`WritePolicy::permissive`] (or `--allow-mojibake` on the CLI,
//! or `"allow_mojibake": true` to the MCP `write_file` tool) to
//! disable the check for legitimate cases such as writing curated
//! mojibake fixtures.  Files that contain the
//! [`crate::mojibake::ALLOW_MARKER`] sentinel are never flagged
//! regardless of policy.

use std::{fs, io::Write, path::Path};

use encoding_rs::Encoding;
use harrier::{
    encoding::{LineEnding, SourceConfig},
    source::Source,
};

use crate::{
    IoMode,
    encoding::{BomPolicy, OutputEncoding},
    mojibake::{WritePolicy, check_write_does_not_introduce_mojibake},
};

/// UTF-8 BOM byte sequence (U+FEFF encoded as UTF-8).
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Run the `write` subcommand.
///
/// Reads all of `inp` as a UTF-8 string (error if not valid UTF-8), detects
/// the target file's encoding and dominant line-ending convention, re-encodes
/// the input bytes, and writes the result atomically.
///
/// If `file` exists: the original is renamed to `<file>.bak` before the new
/// content is placed at `file`.  If it does not exist: parent directories are
/// created as needed and the file is written fresh as UTF-8/LF.
///
/// `output_encoding` and `bom_policy` work together:
/// - [`OutputEncoding::Preserve`] (default): write using the existing file's
///   encoding; BOM policy is ignored.
/// - [`OutputEncoding::Utf8`]: force UTF-8 output regardless of the existing
///   file's encoding.  `bom_policy` then controls whether a UTF-8 BOM is
///   prepended: `Strip` (default) omits it, `Preserve` includes it only if
///   the existing file had one, `Force` always includes it.
///
/// When `line_ending_override` is `Some`, the specified line ending is used
/// for denormalisation instead of the file's detected dominant ending.  The
/// file's encoding is still detected and preserved (or overridden by
/// `output_encoding`).
///
/// When `diff_out` is `Some`, a unified text diff (in LF-normalised UTF-8
/// space) of the old file vs. the new content is written after a successful
/// write.
///
/// `policy` controls write-time content checks.  By default
/// ([`WritePolicy::default`]) the write is rejected if `content` would
/// introduce mojibake matches not present in the file's prior decoded
/// content; pass [`WritePolicy::permissive`] (or the CLI's
/// `--allow-mojibake`) to skip the check.
#[allow(clippy::too_many_arguments)]
pub fn run(
    file: &Path,
    content: &str,
    output_encoding: OutputEncoding,
    bom_policy: BomPolicy,
    line_ending_override: Option<LineEnding>,
    diff_out: Option<&mut dyn Write>,
    io_mode: IoMode,
    policy: WritePolicy,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = crate::recover_stranded_backup(file);
    // Capture old bytes if needed for diff computation OR the mojibake guard.
    let need_old_bytes = diff_out.is_some() || (policy.reject_introduced_mojibake && file.exists());
    let old_bytes: Option<Vec<u8>> = if need_old_bytes && file.exists() {
        Some(crate::retry_io(|| fs::read(file))?)
    } else {
        None
    };

    let utf8_text = content;

    // Detect the target file's encoding, line-ending, and whether a BOM was
    // present (needed for BomPolicy::Preserve).
    let (detected_encoding, detected_le, source_had_bom) = detect_target(file, io_mode)?;
    let target_le = line_ending_override.unwrap_or(detected_le);

    // Mojibake write-time guard.  Decoding the old bytes via the detected
    // encoding gives a UTF-8 string we can compare with `content`.
    if policy.reject_introduced_mojibake {
        let old_decoded: std::borrow::Cow<'_, str> = match &old_bytes {
            Some(bytes) => detected_encoding.decode(bytes).0,
            None => std::borrow::Cow::Borrowed(""),
        };
        check_write_does_not_introduce_mojibake(&old_decoded, content)
            .map_err(|e| format!("write: {}: {e}", file.display()))?;
    }

    let target_encoding = match output_encoding {
        OutputEncoding::Preserve => detected_encoding,
        OutputEncoding::Utf8 => encoding_rs::UTF_8,
    };

    // Only act on bom_policy when --utf8 is active.
    let write_bom = match output_encoding {
        OutputEncoding::Preserve => false,
        OutputEncoding::Utf8 => match bom_policy {
            BomPolicy::Strip => false,
            BomPolicy::Preserve => source_had_bom,
            BomPolicy::Force => true,
        },
    };

    // Encode the UTF-8/LF input to the target encoding's byte representation.
    //
    // encoding_rs::UTF_16LE / UTF_16BE are decode-only encodings per the
    // WHATWG Encoding spec: calling encode() on them silently falls back to
    // UTF-8.  We therefore handle these two encodings manually so that the
    // byte representation is correct for subsequent denormalisation.
    let encoded: std::borrow::Cow<[u8]> = if target_encoding == encoding_rs::UTF_16LE {
        std::borrow::Cow::Owned(
            utf8_text
                .encode_utf16()
                .flat_map(|cu| cu.to_le_bytes())
                .collect(),
        )
    } else if target_encoding == encoding_rs::UTF_16BE {
        std::borrow::Cow::Owned(
            utf8_text
                .encode_utf16()
                .flat_map(|cu| cu.to_be_bytes())
                .collect(),
        )
    } else {
        target_encoding.encode(utf8_text).0
    };

    // Denormalise: substitute each LF code unit with the target line ending.
    let encoded_bytes = match target_le {
        LineEnding::Lf => encoded.into_owned(),
        LineEnding::CrLf => crate::encoding::denormalize_lf_to_crlf(&encoded, target_encoding),
        LineEnding::Cr => crate::encoding::denormalize_lf_to_cr(&encoded, target_encoding),
    };

    // Prepend BOM if required.
    let output_bytes: Vec<u8> = if write_bom {
        let mut v = Vec::with_capacity(UTF8_BOM.len() + encoded_bytes.len());
        v.extend_from_slice(UTF8_BOM);
        v.extend_from_slice(&encoded_bytes);
        v
    } else {
        encoded_bytes
    };

    // Atomic write via the shared temp→.bak→persist→restore helper.
    crate::atomic_write(file, &output_bytes)?;

    // Emit the text diff after a successful write.
    if let (Some(out), Some(old)) = (diff_out, old_bytes) {
        emit_text_diff(file, &old, detected_encoding, utf8_text, out)?;
    }

    Ok(())
}

/// Encode `content` as it would be written to a **brand-new** file — i.e. as
/// if [`detect_target`] had returned its non-existent-file defaults of
/// UTF-8, LF, and no BOM.
///
/// Shared by [`run`]'s own new-file path (reached implicitly whenever `file`
/// does not exist, via [`detect_target`]) and by
/// [`crate::cmd::create::run`], which never has an existing file to detect
/// against and so needs the same defaults without going through the full
/// read/detect machinery.
pub(crate) fn encode_new_file_content(
    content: &str,
    output_encoding: OutputEncoding,
    bom_policy: BomPolicy,
    line_ending_override: Option<LineEnding>,
) -> Vec<u8> {
    // A file that doesn't exist yet is always treated as UTF-8/LF/no-BOM by
    // `detect_target`, so both `OutputEncoding` variants resolve to UTF-8
    // output here and `write_bom` only fires for `Utf8` + `BomPolicy::Force`.
    let write_bom = output_encoding == OutputEncoding::Utf8 && bom_policy == BomPolicy::Force;

    let encoded_bytes = match line_ending_override.unwrap_or(LineEnding::Lf) {
        LineEnding::Lf => content.as_bytes().to_vec(),
        LineEnding::CrLf => {
            crate::encoding::denormalize_lf_to_crlf(content.as_bytes(), encoding_rs::UTF_8)
        }
        LineEnding::Cr => {
            crate::encoding::denormalize_lf_to_cr(content.as_bytes(), encoding_rs::UTF_8)
        }
    };

    if write_bom {
        let mut v = Vec::with_capacity(UTF8_BOM.len() + encoded_bytes.len());
        v.extend_from_slice(UTF8_BOM);
        v.extend_from_slice(&encoded_bytes);
        v
    } else {
        encoded_bytes
    }
}

/// Detect the encoding, dominant line-ending, and BOM presence of `file`.
///
/// Returns `(UTF-8, LF, false)` for new (non-existent) or empty files.
fn detect_target(
    file: &Path,
    io_mode: IoMode,
) -> Result<(&'static Encoding, LineEnding, bool), Box<dyn std::error::Error>> {
    if !file.exists() {
        return Ok((encoding_rs::UTF_8, LineEnding::Lf, false));
    }

    let f = crate::retry_io(|| fs::File::open(file))?;
    // Check length before opening; mapping a 0-byte file is platform-dependent.
    if f.metadata()?.len() == 0 {
        return Ok((encoding_rs::UTF_8, LineEnding::Lf, false));
    }
    drop(f);

    let branch = crate::open_as_branch(file, io_mode)?;
    let source = Source::new(branch, SourceConfig::default())?;
    let had_bom = source.bom_len() > 0;
    Ok((source.encoding(), source.line_ending(), had_bom))
}

// ── Diff helpers ──────────────────────────────────────────────────────────────

/// Write a unified text diff of `old_bytes` → `new_text` to `out`.
///
/// `old_bytes` are decoded using `old_encoding` and their line endings are
/// normalised to LF before comparison.  `new_text` is already UTF-8/LF.
pub(crate) fn emit_text_diff(
    file: &Path,
    old_bytes: &[u8],
    old_encoding: &'static encoding_rs::Encoding,
    new_text: &str,
    out: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    // Decode old bytes using the detected encoding (BOM handling is automatic).
    let (old_decoded, _, _) = old_encoding.decode(old_bytes);
    let old_normalized = old_decoded.replace("\r\n", "\n").replace('\r', "\n");
    let label = file.to_string_lossy();
    let diff = similar::TextDiff::from_lines(old_normalized.as_str(), new_text);
    let text = diff
        .unified_diff()
        .header(&format!("a/{label}"), &format!("b/{label}"))
        .to_string();
    out.write_all(text.as_bytes())?;
    Ok(())
}

/// Write a unified diff of the escaped representations of `old_bytes` and
/// `new_bytes` to `out`.
///
/// Both byte slices are escaped with [`crate::escape::encode_bytes`] and
/// treated as single-line strings for diffing.  This gives a compact,
/// human-readable diff for binary content.
pub(crate) fn emit_binary_diff(
    file: &Path,
    old_bytes: &[u8],
    new_bytes: &[u8],
    out: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let old_encoded = crate::escape::encode_bytes(old_bytes);
    let new_encoded = crate::escape::encode_bytes(new_bytes);
    let label = file.to_string_lossy();
    // Add a trailing newline so similar treats each as a complete line.
    let old_str = format!("{old_encoded}\n");
    let new_str = format!("{new_encoded}\n");
    let diff = similar::TextDiff::from_lines(old_str.as_str(), new_str.as_str());
    let text = diff
        .unified_diff()
        .header(&format!("a/{label}"), &format!("b/{label}"))
        .to_string();
    out.write_all(text.as_bytes())?;
    Ok(())
}

/// Run the `write --binary` subcommand.
///
/// Reads all of `inp` as raw bytes and writes them atomically to `file`
/// with no encoding or line-ending transformation.  The original file (if
/// any) is renamed to `<file>.bak`.
///
/// When `diff_out` is `Some`, a unified diff of the escaped representations
/// of the old and new byte content is written after a successful write.
pub fn run_binary(
    file: &Path,
    new_bytes: &[u8],
    diff_out: Option<&mut dyn Write>,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = crate::recover_stranded_backup(file);

    // Read old bytes for diff (before any overwrite).
    let old_bytes: Option<Vec<u8>> = if diff_out.is_some() && file.exists() {
        Some(crate::retry_io(|| fs::read(file))?)
    } else {
        None
    };

    // Atomic write via the shared temp→.bak→persist→restore helper.  This
    // creates parent directories first and wraps every filesystem mutation in
    // `retry_io` for AV resilience — matching the text path.
    crate::atomic_write(file, new_bytes)?;

    // Emit the binary diff after a successful write.
    if let (Some(out), Some(old)) = (diff_out, old_bytes) {
        emit_binary_diff(file, &old, new_bytes, out)?;
    }

    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use tempfile::NamedTempFile;

    use super::*;

    /// Test-only wrapper that forwards to `run()` with `IoMode::Mmap`.
    fn run_test(
        file: &Path,
        content: &str,
        output_encoding: OutputEncoding,
        bom_policy: BomPolicy,
        line_ending_override: Option<LineEnding>,
        diff_out: Option<&mut dyn Write>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        run(
            file,
            content,
            output_encoding,
            bom_policy,
            line_ending_override,
            diff_out,
            IoMode::Mmap,
            crate::mojibake::WritePolicy::permissive(),
        )
    }

    fn write_text(existing: Option<&[u8]>, input: &str) -> Vec<u8> {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        if let Some(content) = existing {
            fs::write(&path, content).unwrap();
        } else {
            // Remove so write creates it fresh.
            let _ = fs::remove_file(&path);
        }
        run_test(
            &path,
            input,
            OutputEncoding::Preserve,
            BomPolicy::Strip,
            None,
            None,
        )
        .unwrap();
        let result = fs::read(&path).unwrap();
        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&bak);
        let _ = fs::remove_file(&path);
        result
    }

    // ── run: normal text write cases ──────────────────────────────────────────

    #[test]
    fn write_creates_new_utf8_lf_file() {
        let result = write_text(None, "hello\n");
        assert_eq!(result, b"hello\n");
    }

    #[test]
    fn write_preserves_crlf_line_ending() {
        let result = write_text(Some(b"old\r\n"), "new\n");
        assert_eq!(result, b"new\r\n");
    }

    #[test]
    fn write_preserves_bak_on_overwrite() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        fs::write(&path, b"original\n").unwrap();
        run_test(
            &path,
            "replaced\n",
            OutputEncoding::Preserve,
            BomPolicy::Strip,
            None,
            None,
        )
        .unwrap();
        let bak = format!("{}.bak", path.display());
        assert_eq!(fs::read(&bak).unwrap(), b"original\n");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }

    // ── run: diff_out text diff cases ─────────────────────────────────────────

    #[test]
    fn write_text_diff_shows_change() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        fs::write(&path, b"hello world\n").unwrap();
        let mut diff_buf: Vec<u8> = Vec::new();
        run_test(
            &path,
            "hello Rust\n",
            OutputEncoding::Preserve,
            BomPolicy::Strip,
            None,
            Some(&mut diff_buf),
        )
        .unwrap();
        let diff = String::from_utf8(diff_buf).unwrap();
        assert!(diff.contains("-hello world"), "missing removed: {diff:?}");
        assert!(diff.contains("+hello Rust"), "missing added: {diff:?}");
        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }

    #[test]
    fn write_text_diff_no_change_is_empty() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        fs::write(&path, b"unchanged\n").unwrap();
        let mut diff_buf: Vec<u8> = Vec::new();
        run_test(
            &path,
            "unchanged\n",
            OutputEncoding::Preserve,
            BomPolicy::Strip,
            None,
            Some(&mut diff_buf),
        )
        .unwrap();
        let diff = String::from_utf8(diff_buf).unwrap();
        assert!(!diff.contains("@@"), "expected no diff hunks: {diff:?}");
        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }

    #[test]
    fn write_text_diff_multiple_lines() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        fs::write(&path, b"aaa\nbbb\nccc\n").unwrap();
        let mut diff_buf: Vec<u8> = Vec::new();
        run_test(
            &path,
            "aaa\nXXX\nccc\n",
            OutputEncoding::Preserve,
            BomPolicy::Strip,
            None,
            Some(&mut diff_buf),
        )
        .unwrap();
        let diff = String::from_utf8(diff_buf).unwrap();
        assert!(diff.contains("-bbb"), "missing removed line: {diff:?}");
        assert!(diff.contains("+XXX"), "missing added line: {diff:?}");
        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }

    #[test]
    fn write_text_diff_crlf_file_normalizes() {
        // A CRLF file written with the same LF text should produce no diff.
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        fs::write(&path, b"same\r\n").unwrap();
        let mut diff_buf: Vec<u8> = Vec::new();
        run_test(
            &path,
            "same\n",
            OutputEncoding::Preserve,
            BomPolicy::Strip,
            None,
            Some(&mut diff_buf),
        )
        .unwrap();
        let diff = String::from_utf8(diff_buf).unwrap();
        assert!(
            !diff.contains("@@"),
            "no diff expected (same text): {diff:?}"
        );
        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }

    // ── run_binary: normal cases ───────────────────────────────────────────────

    #[test]
    fn write_binary_creates_new_file_with_raw_bytes() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        let _ = fs::remove_file(&path);
        let content: &[u8] = &[0x00, 0x01, 0xFF, 0xFE, b'a'];
        run_binary(&path, content, None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), content);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn write_binary_no_encoding_transform() {
        // Raw bytes including \r\n are written unchanged (no CRLF treatment).
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        let _ = fs::remove_file(&path);
        let content: &[u8] = b"\r\n\n\r";
        run_binary(&path, content, None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), content);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn write_binary_overwrites_existing_file() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        fs::write(&path, b"old content").unwrap();
        run_binary(&path, b"new", None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }

    #[test]
    fn write_binary_creates_bak_file() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        fs::write(&path, b"original").unwrap();
        run_binary(&path, b"replaced", None).unwrap();
        let bak = format!("{}.bak", path.display());
        assert_eq!(fs::read(&bak).unwrap(), b"original");
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }

    #[test]
    fn write_binary_all_256_bytes_round_trip() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        let _ = fs::remove_file(&path);
        let all: Vec<u8> = (0u8..=255u8).collect();
        run_binary(&path, &all, None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), all);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn write_binary_creates_missing_parent_dirs() {
        // Regression: `run_binary` previously created the temp file before
        // `create_dir_all`, so writing into a not-yet-existent nested directory
        // failed at temp-file creation.  Routing through `atomic_write` creates
        // the parent directory first.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("c").join("data.bin");
        let content: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF];
        run_binary(&path, content, None).unwrap();
        assert_eq!(fs::read(&path).unwrap(), content);
    }

    // ── run_binary: diff output ────────────────────────────────────────────────

    #[test]
    fn write_binary_diff_shows_change() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        fs::write(&path, b"\xFF\x00").unwrap();
        let mut diff_buf: Vec<u8> = Vec::new();
        run_binary(&path, b"\xAB\xCD", Some(&mut diff_buf)).unwrap();
        let diff = String::from_utf8(diff_buf).unwrap();
        // Old = \xff\0, new = \xab\xcd — both must appear in the diff.
        assert!(diff.contains("\\xff"), "missing old bytes: {diff:?}");
        assert!(diff.contains("\\xab"), "missing new bytes: {diff:?}");
        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }

    #[test]
    fn write_binary_diff_no_change_empty_diff_body() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        fs::write(&path, b"same").unwrap();
        let mut diff_buf: Vec<u8> = Vec::new();
        run_binary(&path, b"same", Some(&mut diff_buf)).unwrap();
        let diff = String::from_utf8(diff_buf).unwrap();
        assert!(!diff.contains("@@"), "expected no diff hunks: {diff:?}");
        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }

    #[test]
    fn write_binary_diff_header_contains_filename() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);
        fs::write(&path, b"old").unwrap();
        let mut diff_buf: Vec<u8> = Vec::new();
        run_binary(&path, b"new", Some(&mut diff_buf)).unwrap();
        let diff = String::from_utf8(diff_buf).unwrap();
        assert!(
            diff.contains("a/") && diff.contains("b/"),
            "missing diff header: {diff:?}"
        );
        let bak = format!("{}.bak", path.display());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&bak);
    }
}
