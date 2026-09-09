// Copyright (c) 2026, Michael Grier

//! `tpu append` — append UTF-8/LF text to an existing file, applying Git
//! worktree policy or preserving its detected native format.
//!
//! The file must already exist; for new files use `tpu write` instead.
//! The appended text (UTF-8/LF) is concatenated to the decoded content of the
//! existing file, the combined result is re-encoded, its line endings follow
//! an explicit override, definite Git policy, or the detected convention, and
//! the output is written atomically.
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

use std::{fs, io::Write, path::Path};

use harrier::encoding::LineEnding;

use crate::{
    IoMode,
    mojibake::{WritePolicy, check_write_does_not_introduce_mojibake},
};

/// Run the `append` subcommand.
///
/// # Arguments
/// * `file`                 — path to the file to append to (must already exist)
/// * `new_text`             — UTF-8/LF text to append
/// * `line_ending_override` — when `Some`, denormalise all line endings in the
///   combined output to this style instead of Git policy or the detected ending
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
    let _ = crate::recover_stranded_backup(file);
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

    let git_policy = crate::git::policy_for_path(file)?;
    let (encoding, detected_le, had_bom, decoded_text) = if file_len == 0 {
        (
            git_policy
                .working_tree_encoding
                .as_ref()
                .map_or(encoding_rs::UTF_8, |worktree| worktree.encoding),
            LineEnding::Lf,
            false,
            String::new(),
        )
    } else {
        drop(f);
        let decoded = crate::read_text_file(file, io_mode)?;
        (
            decoded.encoding,
            decoded.line_ending,
            decoded.bom_len > 0,
            decoded.text,
        )
    };

    let combined = format!("{decoded_text}{new_text}");
    let target_le = line_ending_override
        .or_else(|| git_policy.line_ending_for_text(&combined))
        .unwrap_or(detected_le);

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

    let encoded = crate::encoding::encode_text_strict(&combined, encoding)
        .map_err(|error| format!("append: {}: {error}", file.display()))?;

    // ── Denormalise LF → target line ending ───────────────────────────────────
    let encoded_bytes = match target_le {
        LineEnding::Lf => encoded,
        LineEnding::CrLf => crate::encoding::denormalize_lf_to_crlf(&encoded, encoding),
        LineEnding::Cr => crate::encoding::denormalize_lf_to_cr(&encoded, encoding),
    };

    // ── Re-prepend BOM if the original file had one ───────────────────────────
    let output_bytes: Vec<u8> = if git_policy.write_bom(had_bom) {
        let bom: &[u8] = crate::encoding::bom_bytes_for(encoding);
        let mut v = Vec::with_capacity(bom.len() + encoded_bytes.len());
        v.extend_from_slice(bom);
        v.extend_from_slice(&encoded_bytes);
        v
    } else {
        encoded_bytes
    };

    // ── Atomic write via the shared temp→.bak→persist→restore helper ─────────
    crate::atomic_write(file, &output_bytes)?;

    Ok(())
}
