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

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use encoding_rs::Encoding;
use harrier::{
    encoding::{LineEnding, SourceConfig},
    source::Source,
};
use tempfile::NamedTempFile;

use crate::{
    encoding::{BomPolicy, OutputEncoding},
    mojibake::{check_write_does_not_introduce_mojibake, WritePolicy},
    IoMode,
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
    // Capture old bytes if needed for diff computation OR the mojibake guard.
    let need_old_bytes = diff_out.is_some()
        || (policy.reject_introduced_mojibake && file.exists());
    let old_bytes: Option<Vec<u8>> = if need_old_bytes && file.exists() {
        Some(fs::read(file)?)
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
        LineEnding::CrLf => denormalize_lf_to_crlf(&encoded, target_encoding),
        LineEnding::Cr => denormalize_lf_to_cr(&encoded, target_encoding),
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

    // Write atomically via a temp file in the same directory (same filesystem).
    // For new files, ensure parent directories exist before creating the temp
    // file (NamedTempFile::new_in fails if the directory doesn't exist yet).
    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    if !file.exists() {
        fs::create_dir_all(dir)?;
    }

    let mut tmp = NamedTempFile::new_in(dir)?;
    tmp.write_all(&output_bytes)?;
    tmp.flush()?;

    if file.exists() {
        let bak = PathBuf::from(format!("{}.bak", file.display()));
        fs::rename(file, &bak)?;
        if let Err(e) = tmp.persist(file) {
            let _ = fs::rename(&bak, file); // attempt to restore
            return Err(e.error.into());
        }
    } else {
        tmp.persist(file).map_err(|e| e.error)?;
    }

    // Emit the text diff after a successful write.
    if let (Some(out), Some(old)) = (diff_out, old_bytes) {
        emit_text_diff(file, &old, detected_encoding, utf8_text, out)?;
    }

    Ok(())
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

    let f = fs::File::open(file)?;
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

// ── LF denormalisation helpers ────────────────────────────────────────────────
//
// Each helper replaces logical LF code units in `bytes` — which are in the
// native representation for the given encoding — with CRLF or CR.
//
// For UTF-16LE:  LF = [0x0A, 0x00]       CRLF = [0x0D, 0x00, 0x0A, 0x00]
// For UTF-16BE:  LF = [0x00, 0x0A]       CRLF = [0x00, 0x0D, 0x00, 0x0A]
// All others:    LF = [0x0A]              CRLF = [0x0D, 0x0A]
//
// The input bytes come directly from encoding_rs::Encoding::encode(), so they
// contain only LF (not CRLF) because the source string had '\n' characters.
// There is therefore no risk of double-inserting CR before an existing CR.

fn denormalize_lf_to_crlf(bytes: &[u8], encoding: &'static Encoding) -> Vec<u8> {
    if encoding == encoding_rs::UTF_16LE {
        replace_u16_pairs(bytes, [0x0A, 0x00], &[0x0D, 0x00, 0x0A, 0x00])
    } else if encoding == encoding_rs::UTF_16BE {
        replace_u16_pairs(bytes, [0x00, 0x0A], &[0x00, 0x0D, 0x00, 0x0A])
    } else {
        insert_cr_before_lf(bytes)
    }
}

fn denormalize_lf_to_cr(bytes: &[u8], encoding: &'static Encoding) -> Vec<u8> {
    if encoding == encoding_rs::UTF_16LE {
        replace_u16_pairs(bytes, [0x0A, 0x00], &[0x0D, 0x00])
    } else if encoding == encoding_rs::UTF_16BE {
        replace_u16_pairs(bytes, [0x00, 0x0A], &[0x00, 0x0D])
    } else {
        bytes
            .iter()
            .map(|&b| if b == 0x0A { 0x0D } else { b })
            .collect()
    }
}

/// Scan `bytes` in 2-byte (UTF-16 code-unit) steps, replacing every
/// occurrence of the 2-byte `needle` with `replacement`.
///
/// Bytes that do not match the needle are forwarded one byte at a time.
/// This handles any odd-length tail (which should not occur in valid UTF-16
/// but is forwarded safely rather than silently dropped).
fn replace_u16_pairs(bytes: &[u8], needle: [u8; 2], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 20);
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == needle[0] && bytes[i + 1] == needle[1] {
            out.extend_from_slice(replacement);
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    if i < bytes.len() {
        out.push(bytes[i]);
    }
    out
}

/// Insert `\r` (0x0D) before each `\n` (0x0A) byte in a single-byte or
/// multi-byte UTF-8 stream.
///
/// Only `\n` bytes are targeted; no byte in a valid UTF-8 or single-byte
/// encoding can be confused with `\n` (0x0A is never a continuation byte).
fn insert_cr_before_lf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 20);
    for &b in bytes {
        if b == 0x0A {
            out.push(0x0D);
        }
        out.push(b);
    }
    out
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
    // Read old bytes for diff (before any overwrite).
    let old_bytes: Option<Vec<u8>> = if diff_out.is_some() && file.exists() {
        Some(fs::read(file)?)
    } else {
        None
    };

    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut tmp = NamedTempFile::new_in(dir)?;
    tmp.write_all(new_bytes)?;
    tmp.flush()?;

    if file.exists() {
        let bak = PathBuf::from(format!("{}.bak", file.display()));
        fs::rename(file, &bak)?;
        if let Err(e) = tmp.persist(file) {
            let _ = fs::rename(&bak, file);
            return Err(e.error.into());
        }
    } else {
        if let Some(parent) = file.parent()
            && !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        tmp.persist(file).map_err(|e| e.error)?;
    }

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
