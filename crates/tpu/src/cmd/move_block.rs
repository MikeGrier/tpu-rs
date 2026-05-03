// Copyright (c) 2026, Michael Grier

//! `tpu move-block` — move a contiguous block of lines from one file to another.
//!
//! The source file is read as raw bytes and interpreted as UTF-8 text.
//! Line terminators (LF or CRLF) are preserved in the moved content.
//! The block is removed from the source file and appended to the destination
//! file (which is created if absent).  An optional dest_header line may be
//! prepended to the block in the destination.

use std::{error::Error, fs, path::Path};

use regex::Regex;

/// Result of a successful `move_block` operation.
#[derive(Debug)]
pub struct MoveResult {
    /// Absolute or relative path to the source file, as supplied.
    pub source_file: String,
    /// Absolute or relative path to the destination file, as supplied.
    pub dest_file: String,
    /// Number of lines removed from source and appended to dest.
    pub moved_lines: usize,
}

/// Move a contiguous block of lines from `source` to `dest`.
///
/// The block begins at the first line matching `start_pat` (inclusive) and
/// ends just before the first subsequent line matching `end_pat` (exclusive),
/// or at EOF when `end_pat` is `None`.
///
/// If `dest_header` is supplied it is prepended (with the source file's
/// dominant line ending) to the moved block in the destination file.
///
/// Both files are plain UTF-8 text.  Line endings are preserved verbatim in
/// the moved content; any freshly-synthesised line (the separator or header)
/// uses the source file's dominant line ending.
///
/// Returns an error if:
/// * Either regex is invalid.
/// * `start_pat` is not found in the source file.
/// * Either file cannot be read or written.
/// * The source file is not valid UTF-8.
pub fn run(
    source: &Path,
    dest: &Path,
    start_pat: &str,
    end_pat: Option<&str>,
    dest_header: Option<&str>,
) -> Result<MoveResult, Box<dyn Error>> {
    let start_re = Regex::new(start_pat).map_err(|e| format!("invalid --start-pattern: {e}"))?;
    let end_re = end_pat
        .map(|p| Regex::new(p).map_err(|e| format!("invalid --end-pattern: {e}")))
        .transpose()?;

    // Read source as raw bytes; require UTF-8.
    let source_bytes = fs::read(source)?;
    let source_text = String::from_utf8(source_bytes)
        .map_err(|_| format!("source file is not valid UTF-8: {}", source.display()))?;

    // Detect dominant line ending to use for any synthesised lines.
    let native_le: &str = if source_text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };

    // Split into raw lines, each retaining its own terminator.
    let raw_lines = split_raw_lines(&source_text);

    // Stripped (no-terminator) view for regex matching.
    let stripped: Vec<&str> = raw_lines
        .iter()
        .map(|l| l.trim_end_matches(['\r', '\n']))
        .collect();

    // Locate block start.
    let start_idx = stripped
        .iter()
        .position(|l| start_re.is_match(l))
        .ok_or_else(|| {
            format!(
                "start_pattern {:?} not found in {}",
                start_pat,
                source.display()
            )
        })?;

    // Locate block end (exclusive).
    let end_idx = match &end_re {
        Some(re) => stripped[start_idx + 1..]
            .iter()
            .position(|l| re.is_match(l))
            .map(|i| start_idx + 1 + i)
            .unwrap_or(raw_lines.len()),
        None => raw_lines.len(),
    };

    let moved_lines = end_idx - start_idx;

    // Build the text to append to dest.
    let mut moved = String::new();
    if let Some(header) = dest_header {
        moved.push_str(header);
        moved.push_str(native_le);
    }
    for raw in &raw_lines[start_idx..end_idx] {
        moved.push_str(raw);
    }
    // Ensure a trailing newline so subsequent appends start on a fresh line.
    if !moved.ends_with('\n') {
        moved.push_str(native_le);
    }

    // Append to dest (creating it if absent).
    if dest.exists() {
        let dest_bytes = fs::read(dest)?;
        let mut dest_content = String::from_utf8(dest_bytes)
            .map_err(|_| format!("dest file is not valid UTF-8: {}", dest.display()))?;
        // Guarantee dest ends with a newline before appending.
        if !dest_content.ends_with('\n') {
            let dest_le: &str = if dest_content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            dest_content.push_str(dest_le);
        }
        dest_content.push_str(&moved);
        fs::write(dest, dest_content.as_bytes())?;
    } else {
        fs::write(dest, moved.as_bytes())?;
    }

    // Write remaining lines back to source.
    let mut remaining = String::new();
    for (i, raw) in raw_lines.iter().enumerate() {
        if i < start_idx || i >= end_idx {
            remaining.push_str(raw);
        }
    }
    fs::write(source, remaining.as_bytes())?;

    Ok(MoveResult {
        source_file: source.display().to_string(),
        dest_file: dest.display().to_string(),
        moved_lines,
    })
}

/// Split `text` into raw lines, each retaining its own line terminator.
///
/// A final partial line with no terminator is included as-is.
fn split_raw_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = Vec::new();
    let mut pos = 0usize;
    while pos < text.len() {
        let rest = &text[pos..];
        let (content_len, nl_len) = if let Some(i) = rest.find("\r\n") {
            (i, 2usize)
        } else if let Some(i) = rest.find('\n') {
            (i, 1usize)
        } else {
            // Final partial line with no terminator.
            lines.push(rest);
            break;
        };
        lines.push(&rest[..content_len + nl_len]);
        pos += content_len + nl_len;
    }
    lines
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    // ── Helper ────────────────────────────────────────────────────────────────

    fn write(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        fs::write(&p, content.as_bytes()).unwrap();
        p
    }

    // ── Normal cases ──────────────────────────────────────────────────────────

    /// MB-1: Basic move with start+end pattern and dest_header.
    #[test]
    fn mb_1_basic_move_with_header() {
        let d = tempdir().unwrap();
        let src = write(
            d.path(),
            "src.txt",
            "# Header\n\
             ## Keep\n\
             keep line\n\
             ## Move\n\
             move line 1\n\
             move line 2\n\
             ## After\n\
             after line\n",
        );
        let dst = write(d.path(), "dst.txt", "# Dest\n");

        let r = run(
            &src,
            &dst,
            r"^## Move",
            Some(r"^## After"),
            Some("## Moved 2026-01-01"),
        )
        .unwrap();

        assert_eq!(r.moved_lines, 3);
        let src_after = fs::read_to_string(&src).unwrap();
        assert!(
            !src_after.contains("## Move\n"),
            "section moved out of source"
        );
        assert!(src_after.contains("## Keep"), "keep section still present");
        assert!(
            src_after.contains("## After"),
            "after section still present"
        );

        let dst_after = fs::read_to_string(&dst).unwrap();
        assert!(
            dst_after.contains("## Moved 2026-01-01"),
            "dest_header in dest"
        );
        assert!(dst_after.contains("## Move\n"), "original heading in dest");
        assert!(dst_after.contains("move line 1"), "move line 1 in dest");
        assert!(dst_after.contains("move line 2"), "move line 2 in dest");
        assert!(!dst_after.contains("## Keep"), "keep section not in dest");
    }

    /// MB-2: No end_pattern; block runs to EOF.
    #[test]
    fn mb_2_no_end_pattern_eof() {
        let d = tempdir().unwrap();
        let src = write(
            d.path(),
            "src.txt",
            "preamble\n\
             ## Section\n\
             item a\n\
             item b\n",
        );
        let dst = d.path().join("dst.txt");

        let r = run(&src, &dst, r"^## Section", None, None).unwrap();

        assert_eq!(r.moved_lines, 3);
        let src_after = fs::read_to_string(&src).unwrap();
        assert!(src_after.contains("preamble"), "preamble remains");
        assert!(
            !src_after.contains("## Section"),
            "section gone from source"
        );

        let dst_after = fs::read_to_string(&dst).unwrap();
        assert!(dst_after.contains("## Section"), "section in dest");
        assert!(dst_after.contains("item a"), "item a in dest");
    }

    /// MB-3: Dest file does not exist; should be created.
    #[test]
    fn mb_3_dest_created() {
        let d = tempdir().unwrap();
        let src = write(d.path(), "src.txt", "line one\nline two\n");
        let dst = d.path().join("nonexistent.txt");
        assert!(!dst.exists(), "precondition: dest must not exist");

        let r = run(&src, &dst, r"line two", None, None).unwrap();

        assert_eq!(r.moved_lines, 1);
        assert!(dst.exists(), "dest was created");
        assert!(fs::read_to_string(&dst).unwrap().contains("line two"));
    }

    /// MB-4: Dest file already has content; block is appended.
    #[test]
    fn mb_4_appended_to_existing_dest() {
        let d = tempdir().unwrap();
        let src = write(d.path(), "src.txt", "alpha\nbeta\ngamma\n");
        let dst = write(d.path(), "dst.txt", "existing content\n");

        run(&src, &dst, r"^beta", None, None).unwrap();

        let dst_after = fs::read_to_string(&dst).unwrap();
        assert!(
            dst_after.starts_with("existing content\n"),
            "original dest preserved"
        );
        assert!(dst_after.contains("beta"), "moved line appended");
    }

    /// MB-5: Move single line (start and end are adjacent sections).
    #[test]
    fn mb_5_single_line_block() {
        let d = tempdir().unwrap();
        let src = write(d.path(), "src.txt", "before\ntarget line\nafter\n");
        let dst = d.path().join("dst.txt");

        let r = run(&src, &dst, r"target line", Some(r"after"), None).unwrap();

        assert_eq!(r.moved_lines, 1);
        let src_after = fs::read_to_string(&src).unwrap();
        assert!(
            !src_after.contains("target line"),
            "target removed from source"
        );
        assert!(src_after.contains("before"), "before remains");
        assert!(src_after.contains("after"), "after remains");
    }

    /// MB-6: CRLF source; line endings preserved in moved content.
    #[test]
    fn mb_6_crlf_preserved() {
        let d = tempdir().unwrap();
        let src_path = d.path().join("src.txt");
        let src_content = "line1\r\nMOVE\r\nline3\r\n";
        fs::write(&src_path, src_content.as_bytes()).unwrap();
        let dst = d.path().join("dst.txt");

        run(&src_path, &dst, r"^MOVE", None, None).unwrap();

        let dst_bytes = fs::read(&dst).unwrap();
        assert!(
            dst_bytes.windows(2).any(|w| w == b"\r\n"),
            "CRLF preserved in dest"
        );
        let dst_str = String::from_utf8(dst_bytes).unwrap();
        assert!(dst_str.contains("MOVE\r\n"), "MOVE line with CRLF in dest");
    }

    /// MB-7: No dest_header; appended directly without extra line.
    #[test]
    fn mb_7_no_dest_header() {
        let d = tempdir().unwrap();
        let src = write(d.path(), "src.txt", "stay\ngo\n");
        let dst = write(d.path(), "dst.txt", "already\n");

        run(&src, &dst, r"^go", None, None).unwrap();

        let dst_after = fs::read_to_string(&dst).unwrap();
        assert_eq!(dst_after, "already\ngo\n", "no extra header line inserted");
    }

    /// MB-8: Multiple potential end matches; only the first one after start is used.
    #[test]
    fn mb_8_first_end_match_wins() {
        let d = tempdir().unwrap();
        let src = write(
            d.path(),
            "src.txt",
            "## A\nstay a1\nstay a2\n## B\nmove b1\n## C\nstay c1\n## D\nstay d1\n",
        );
        let dst = d.path().join("dst.txt");

        let r = run(&src, &dst, r"^## B", Some(r"^## [A-Z]"), None).unwrap();

        // Block is "## B\nmove b1\n" — 2 lines; ## C ends the block.
        assert_eq!(r.moved_lines, 2);
        let src_after = fs::read_to_string(&src).unwrap();
        assert!(!src_after.contains("## B"), "section B gone");
        assert!(src_after.contains("## C"), "section C still present");
        assert!(src_after.contains("## D"), "section D still present");
    }

    /// MB-9: Move the first section (start at beginning of file).
    #[test]
    fn mb_9_first_section_moved() {
        let d = tempdir().unwrap();
        let src = write(
            d.path(),
            "src.txt",
            "## First\nitem f1\n## Second\nitem s1\n",
        );
        let dst = d.path().join("dst.txt");

        let r = run(&src, &dst, r"^## First", Some(r"^## Second"), None).unwrap();

        assert_eq!(r.moved_lines, 2);
        let src_after = fs::read_to_string(&src).unwrap();
        assert!(
            src_after.starts_with("## Second"),
            "second section now at top"
        );
        assert!(!src_after.contains("## First"), "first section gone");
    }

    /// MB-10: Near-end section; leaves only a small remainder in source.
    #[test]
    fn mb_10_near_end_section() {
        let d = tempdir().unwrap();
        let src = write(d.path(), "src.txt", "preamble\nmore\n## Last\nlast item\n");
        let dst = d.path().join("dst.txt");

        let r = run(&src, &dst, r"^## Last", None, None).unwrap();

        assert_eq!(r.moved_lines, 2);
        let src_after = fs::read_to_string(&src).unwrap();
        assert_eq!(src_after, "preamble\nmore\n");
    }

    /// MB-11: dest does not end with newline; separator added before moved content.
    #[test]
    fn mb_11_dest_missing_trailing_newline() {
        let d = tempdir().unwrap();
        let src = write(d.path(), "src.txt", "stay\ngo\n");
        let dst_path = d.path().join("dst.txt");
        // Write dest WITHOUT a trailing newline.
        fs::write(&dst_path, b"no-newline-at-end").unwrap();

        run(&src, &dst_path, r"^go", None, None).unwrap();

        let dst_after = fs::read(&dst_path).unwrap();
        let dst_str = String::from_utf8(dst_after).unwrap();
        // The tool must not concatenate dest content and moved line without a newline.
        assert!(
            !dst_str.contains("no-newline-at-endgo"),
            "newline added before appended content; got: {dst_str:?}"
        );
        assert!(dst_str.contains("go"), "moved content present");
    }

    /// MB-12: source has a line with no trailing newline at EOF.
    #[test]
    fn mb_12_source_no_trailing_newline() {
        let d = tempdir().unwrap();
        let src_path = d.path().join("src.txt");
        // Last line has no newline.
        fs::write(&src_path, b"keep\ngo").unwrap();
        let dst = d.path().join("dst.txt");

        let r = run(&src_path, &dst, r"^go", None, None).unwrap();

        assert_eq!(r.moved_lines, 1);
        let src_after = fs::read_to_string(&src_path).unwrap();
        assert_eq!(src_after, "keep\n");
        let dst_after = fs::read_to_string(&dst).unwrap();
        assert!(dst_after.contains("go"), "partial line moved");
    }

    // ── Error cases ───────────────────────────────────────────────────────────

    /// MB-E1: start_pattern not found → error.
    #[test]
    fn mb_e1_start_pattern_not_found() {
        let d = tempdir().unwrap();
        let src = write(d.path(), "src.txt", "line one\nline two\n");
        let dst = d.path().join("dst.txt");

        let err = run(&src, &dst, "DOES_NOT_EXIST", None, None)
            .expect_err("must fail when pattern missing");
        assert!(
            err.to_string().contains("not found"),
            "error mentions 'not found': {err}"
        );
        // Source must be untouched.
        assert_eq!(
            fs::read_to_string(&src).unwrap(),
            "line one\nline two\n",
            "source unchanged on error"
        );
        // Dest must not have been created.
        assert!(!dst.exists(), "dest not created on error");
    }

    /// MB-E2: invalid start_pattern regex → error before any I/O.
    #[test]
    fn mb_e2_invalid_start_regex() {
        let d = tempdir().unwrap();
        let src = write(d.path(), "src.txt", "hello\n");
        let dst = d.path().join("dst.txt");

        let err = run(&src, &dst, r"[invalid", None, None).expect_err("must fail on invalid regex");
        assert!(
            err.to_string().contains("invalid --start-pattern"),
            "error mentions start-pattern: {err}"
        );
        assert!(!dst.exists(), "dest not created on invalid regex");
    }

    /// MB-E3: invalid end_pattern regex → error before any I/O.
    #[test]
    fn mb_e3_invalid_end_regex() {
        let d = tempdir().unwrap();
        let src = write(d.path(), "src.txt", "hello\nworld\n");
        let dst = d.path().join("dst.txt");

        let err = run(&src, &dst, r"hello", Some(r"[invalid"), None)
            .expect_err("must fail on invalid end regex");
        assert!(
            err.to_string().contains("invalid --end-pattern"),
            "error mentions end-pattern: {err}"
        );
        assert!(!dst.exists(), "dest not created on invalid end regex");
    }
}
