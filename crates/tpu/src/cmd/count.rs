// Copyright (c) 2026, Michael Grier

//! `tpu count` — count lines, words, chars, bytes, and regex pattern occurrences.
//!
//! When no metric flag is set all four standard metrics (lines, words, chars,
//! bytes) are reported.  Each `--pattern` value adds an additional named count,
//! emitted after the standard metrics in declaration order.
//!
//! When `stats` is true (from `--stats` or JSON mode), encoding metadata is
//! emitted first: the WHATWG encoding name, BOM presence, and line-ending style.

use std::{fs, path::Path, sync::Arc};

use harrier::{
    encoding::{LineEnding, SourceConfig},
    source::Source,
};
use regex::Regex;

use crate::{output::Output, IoMode};

/// Run the `count` subcommand.
///
/// # Arguments
/// * `file`     — path to the file to inspect
/// * `lines`    — count logical (encoding-aware) lines
/// * `words`    — count whitespace-delimited tokens in the decoded text
/// * `chars`    — count Unicode scalar values in the decoded text
/// * `bytes`    — count raw bytes (file size on disk)
/// * `patterns` — zero or more Rust regex strings; each produces one count
/// * `labels`   — human-readable label for each pattern (positionally aligned;
///   missing labels default to the pattern string; surplus labels
///   are an error)
/// * `stats`    — emit encoding name, BOM presence, and line-ending style
///   before the metric counts; always true in JSON mode
/// * `out`      — output sink (human or JSON, driven by `--message-format`)
#[allow(clippy::too_many_arguments)]
pub fn run(
    file: &Path,
    lines: bool,
    words: bool,
    chars: bool,
    bytes: bool,
    patterns: &[String],
    labels: &[String],
    stats: bool,
    out: &mut dyn Output,
    io_mode: IoMode,
) -> Result<(), Box<dyn std::error::Error>> {
    // Validate: surplus labels are an error.
    if labels.len() > patterns.len() {
        return Err(format!(
            "count: {} --label value{} supplied but only {} --pattern value{} given",
            labels.len(),
            if labels.len() == 1 { "" } else { "s" },
            patterns.len(),
            if patterns.len() == 1 { "" } else { "s" },
        )
        .into());
    }

    // Determine which standard metrics to emit.  When none are requested,
    // emit all four.
    let any_standard = lines || words || chars || bytes;
    let emit_lines = lines || !any_standard;
    let emit_words = words || !any_standard;
    let emit_chars = chars || !any_standard;
    let emit_bytes = bytes || !any_standard;

    // ── Raw byte count (file size on disk) ───────────────────────────────────
    // Obtain this before mmapping so we have the size even for empty files.
    let byte_count = fs::metadata(file)?.len();

    // ── Open and decode the file, capturing file metadata ────────────────────
    let (text, enc_name, has_bom, line_ending_label): (String, &'static str, bool, &'static str) =
        if byte_count == 0 {
            // Empty file — no encoding can be detected; use UTF-8 defaults.
            (String::new(), "UTF-8", false, "LF")
        } else {
            let branch = crate::open_as_branch(file, io_mode)?;
            let file_len = branch.byte_len();
            let source = Source::new(Arc::clone(&branch), SourceConfig::default())?;
            let bom_len = source.bom_len();
            let encoding = source.encoding();
            // Capture line_ending BEFORE as_lines() consumes `source`.
            let line_ending = source.line_ending();
            let lines_iter = source.as_lines()?;
            // Skip BOM bytes so decoded text starts at the first content char.
            let view = lines_iter.view_range(bom_len as u64..file_len)?;
            let (cow, _) = encoding.decode_without_bom_handling(&view.bytes);
            let text = cow.into_owned();

            let enc_name: &'static str = encoding.name();
            let has_bom = bom_len > 0;
            let le_label: &'static str = match line_ending {
                LineEnding::Lf => "LF",
                LineEnding::CrLf => "CRLF",
                LineEnding::Cr => "CR",
            };
            (text, enc_name, has_bom, le_label)
        };

    // ── Emit stats ────────────────────────────────────────────────────────────
    // Stats (encoding, BOM, line-ending) are emitted before metric counts.
    // In human mode this block only runs when --stats was supplied; in JSON
    // mode the caller always sets stats=true so JSON consumers always see them.
    if stats {
        out.emit_json(
            "count",
            None,
            None,
            &serde_json::json!({
                "reason": "data",
                "subcommand": "count",
                "metric": "encoding",
                "value": enc_name,
                "rendered": format!("encoding: {enc_name}\n"),
            }),
        );
        out.emit_json(
            "count",
            None,
            None,
            &serde_json::json!({
                "reason": "data",
                "subcommand": "count",
                "metric": "bom",
                "value": has_bom,
                "rendered": format!("bom: {has_bom}\n"),
            }),
        );
        out.emit_json(
            "count",
            None,
            None,
            &serde_json::json!({
                "reason": "data",
                "subcommand": "count",
                "metric": "line_ending",
                "value": line_ending_label,
                "rendered": format!("line_ending: {line_ending_label}\n"),
            }),
        );
    }

    // ── Standard metrics ─────────────────────────────────────────────────────

    // Line count: split on '\n' (harrier normalises all line endings to LF).
    // A trailing '\n' produces a trailing empty token; discard it.
    let line_count: usize = {
        let parts: Vec<&str> = text.split('\n').collect();
        if parts.last() == Some(&"") {
            parts.len() - 1
        } else {
            parts.len()
        }
    };

    // Word count: split on ASCII whitespace, discard empty tokens.
    let word_count: usize = text.split_ascii_whitespace().count();

    // Char count: Unicode scalar values.
    let char_count: usize = text.chars().count();

    // ── Emit standard metrics ─────────────────────────────────────────────────
    // Human mode: "<label>: <count>\n"
    // JSON mode:  emit_json with "reason":"data","subcommand":"count",
    //             "metric":<label>,"count":<n>,"rendered":"<label>: <n>\n"
    let mut emit_metric = |label: &str, count: u64| {
        let rendered = format!("{label}: {count}\n");
        out.emit_json(
            "count",
            None,
            None,
            &serde_json::json!({
                "reason": "data",
                "subcommand": "count",
                "metric": label,
                "count": count,
                "rendered": rendered,
            }),
        );
    };

    if emit_lines {
        emit_metric("lines", line_count as u64);
    }
    if emit_words {
        emit_metric("words", word_count as u64);
    }
    if emit_chars {
        emit_metric("chars", char_count as u64);
    }
    if emit_bytes {
        emit_metric("bytes", byte_count);
    }

    // ── Pattern counts ────────────────────────────────────────────────────────
    for (i, pat) in patterns.iter().enumerate() {
        let label = labels.get(i).map(String::as_str).unwrap_or(pat.as_str());
        let re = Regex::new(pat).map_err(|e| format!("count: invalid pattern {pat:?}: {e}"))?;
        let count = re.find_iter(&text).count();
        emit_metric(label, count as u64);
    }

    Ok(())
}
