// Copyright (c) 2026, Michael Grier

//! `tpu append` — append UTF-8/LF text to an existing file, preserving its
//! native encoding, BOM, and dominant line-ending convention.
//!
//! The file must already exist; for new files use `tpu write` instead.
//! The appended text (UTF-8/LF) is concatenated to the decoded content of the
//! existing file, the combined result is re-encoded in the original encoding,
//! the line endings are denormalised to match (or to the override), and the
//! output is written atomically.
//!
//! ## Write-time mojibake guard
//!
//! After concatenation and before any bytes touch disk, [`run`]
//! forwards the combined file content through
//! [`crate::mojibake::check_write_does_not_introduce_mojibake`] using
//! the original file's content as the baseline.  An append whose
//! payload introduces *new* mojibake matches is rejected and the file
//! is left untouched.  Pre-existing damage in the file is ignored.
//! Pass [`WritePolicy::permissive`] / `--allow-mojibake` /
//! `"allow_mojibake": true` to override.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use harrier::{
    encoding::{LineEnding, SourceConfig},
    source::Source,
};

use crate::{mojibake::{check_write_does_not_introduce_mojibake, WritePolicy}, IoMode};

/// Run the `append` subcommand.
///
/// # Arguments
/// * `file`                 — path to the file to append to (must already exist)
/// * `new_text`             — UTF-8/LF text to append
/// * `line_ending_override` — when `Some`, denormalise all line endings in the
///   combined output to this style instead of the
///   detected dominant ending of the existing file
/// * `diff_out`             — when `Some`, emit a unified diff of the change
///   to this writer and return without modifying the
///   file (dry-run / preview mode)
/// * `policy`               — write-time mojibake guard ([`WritePolicy::default`]
///   rejects appended content that would introduce new mojibake matches)
pub fn run(
    file: &Path,
    new_text: &str,
    line_ending_override: Option<LineEnding>,
    diff_out: Option<&mut dyn Write>,
    io_mode: IoMode,
    policy: WritePolicy,
) -> Result<(), Box<dyn std::error::Error>> {
    if !file.exists() {
        return Err(format!(
            "append: file does not exist: {}; use 'tpu write' to create new files",
            file.display()
        )
        .into());
    }

    // ── Open the file and detect its properties ───────────────────────────────
    let f = crate::retry_io(|| fs::File::open(file))?;
    let file_len = f.metadata()?.len();

    let (encoding, detected_le, had_bom, decoded_text) = if file_len == 0 {
        // Empty file: default to UTF-8/LF with no BOM.
        (encoding_rs::UTF_8, LineEnding::Lf, false, String::new())
    } else {
        drop(f);

        let branch = crate::open_as_branch(file, io_mode)?;
        let len = branch.byte_len();
        let source = Source::new(Arc::clone(&branch), SourceConfig::default())?;
        let encoding = source.encoding();
        let le = source.line_ending();
        let bom_len = source.bom_len();
        let had_bom = bom_len > 0;

        // Decode content, skipping the BOM bytes.
        let lines_iter = source.as_lines()?;
        let view = lines_iter.view_range(bom_len as u64..len)?;
        let (cow, _) = encoding.decode_without_bom_handling(&view.bytes);
        (encoding, le, had_bom, cow.into_owned())
    };

    let target_le = line_ending_override.unwrap_or(detected_le);
    let combined = format!("{decoded_text}{new_text}");

    // Mojibake write-time guard.  Compare decoded old vs. combined new in
    // UTF-8 char space.  Done here so dry-run also reports the issue.
    if policy.reject_introduced_mojibake {
        check_write_does_not_introduce_mojibake(&decoded_text, &combined)
            .map_err(|e| format!("append: {}: {e}", file.display()))?;
    }

    // ── Diff-only (preview) mode ──────────────────────────────────────────────
    // Emit a unified diff to `diff_out` and return without touching the file.
    if let Some(out) = diff_out {
        let existing_bytes = crate::retry_io(|| fs::read(file))?;
        crate::cmd::write::emit_text_diff(file, &existing_bytes, encoding, &combined, out)?;
        return Ok(());
    }

    // ── Encode the combined text into the target encoding ─────────────────────
    //
    // encoding_rs::UTF_16LE and UTF_16BE are decode-only per the WHATWG
    // Encoding spec; handle them manually so the byte sequence is correct.
    let encoded: Vec<u8> = if encoding == encoding_rs::UTF_16LE {
        combined
            .encode_utf16()
            .flat_map(|cu| cu.to_le_bytes())
            .collect()
    } else if encoding == encoding_rs::UTF_16BE {
        combined
            .encode_utf16()
            .flat_map(|cu| cu.to_be_bytes())
            .collect()
    } else {
        encoding.encode(&combined).0.into_owned()
    };

    // ── Denormalise LF → target line ending ───────────────────────────────────
    let encoded_bytes = match target_le {
        LineEnding::Lf => encoded,
        LineEnding::CrLf => denormalize_lf_to_crlf(&encoded, encoding),
        LineEnding::Cr => denormalize_lf_to_cr(&encoded, encoding),
    };

    // ── Re-prepend BOM if the original file had one ───────────────────────────
    let output_bytes: Vec<u8> = if had_bom {
        let bom: &[u8] = bom_bytes_for(encoding);
        let mut v = Vec::with_capacity(bom.len() + encoded_bytes.len());
        v.extend_from_slice(bom);
        v.extend_from_slice(&encoded_bytes);
        v
    } else {
        encoded_bytes
    };

    // ── Atomic write via temp file in the same directory ─────────────────────
    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(&output_bytes)?;
    tmp.flush()?;

    let bak = PathBuf::from(format!("{}.bak", file.display()));
    crate::retry_io(|| fs::rename(file, &bak))?;
    if let Err(e) = tmp.persist(file) {
        let _ = crate::retry_io(|| fs::rename(&bak, file)); // attempt to restore original
        return Err(e.error.into());
    }

    Ok(())
}

// ── BOM helpers ───────────────────────────────────────────────────────────────

/// Return the BOM byte sequence for `encoding`, or an empty slice if none.
fn bom_bytes_for(encoding: &'static encoding_rs::Encoding) -> &'static [u8] {
    if encoding == encoding_rs::UTF_8 {
        &[0xEF, 0xBB, 0xBF]
    } else if encoding == encoding_rs::UTF_16LE {
        &[0xFF, 0xFE]
    } else if encoding == encoding_rs::UTF_16BE {
        &[0xFE, 0xFF]
    } else {
        &[]
    }
}

// ── LF denormalisation helpers ────────────────────────────────────────────────
//
// These replace logical LF code units (in the target encoding's native
// representation) with CRLF or CR.  The logic mirrors write.rs exactly.
//
// For UTF-16LE:  LF = [0x0A, 0x00]       CRLF = [0x0D, 0x00, 0x0A, 0x00]
// For UTF-16BE:  LF = [0x00, 0x0A]       CRLF = [0x00, 0x0D, 0x00, 0x0A]
// All others:    LF = [0x0A]              CRLF = [0x0D, 0x0A]

fn denormalize_lf_to_crlf(bytes: &[u8], encoding: &'static encoding_rs::Encoding) -> Vec<u8> {
    if encoding == encoding_rs::UTF_16LE {
        replace_u16_pairs(bytes, [0x0A, 0x00], &[0x0D, 0x00, 0x0A, 0x00])
    } else if encoding == encoding_rs::UTF_16BE {
        replace_u16_pairs(bytes, [0x00, 0x0A], &[0x00, 0x0D, 0x00, 0x0A])
    } else {
        insert_cr_before_lf(bytes)
    }
}

fn denormalize_lf_to_cr(bytes: &[u8], encoding: &'static encoding_rs::Encoding) -> Vec<u8> {
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

/// Replace all 2-byte `needle` occurrences in `bytes` with `replacement`.
///
/// Scans in 2-byte steps; any odd trailing byte is forwarded unchanged.
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
/// multi-byte encoding stream.
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
