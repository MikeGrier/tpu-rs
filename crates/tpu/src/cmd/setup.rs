// Copyright (c) 2026, Michael Grier

//! `tpu setup` — emit (or inject) canonical Copilot-instructions guidance
//! for the `tpu-mcp` server's tools.
//!
//! The output is a self-contained Markdown block, delimited by stable
//! HTML-comment markers, that lists every `tpu_*` MCP tool and explains
//! when Copilot should reach for it instead of PowerShell / shell I/O.
//!
//! Two modes:
//!
//! - **emit** (default) — print the block to stdout. Useful for piping into
//!   a fresh `.github/copilot-instructions.md` or for inspecting what would
//!   be written.
//! - **inject** (`--inject <PATH>`) — idempotently merge the block into an
//!   existing file. If the markers already exist their contents are
//!   replaced; otherwise the block is appended (with a leading blank line
//!   if the file does not already end in one). Either way the file's
//!   encoding and dominant line-ending convention are preserved by routing
//!   the write through [`crate::cmd::write::run`].
//!
//! The injected block is bounded by:
//!
//! ```text
//! <!-- tpu-mcp:setup:begin -->
//! ... generated content ...
//! <!-- tpu-mcp:setup:end -->
//! ```
//!
//! This lets `tpu setup --inject` be re-run safely after upgrading the
//! tool: the block is always rewritten to the current canonical form
//! without disturbing surrounding content.

use std::{
    fs,
    path::Path,
    sync::Arc,
};

use harrier::{encoding::SourceConfig, source::Source};

use crate::{
    encoding::{BomPolicy, OutputEncoding},
    mojibake::WritePolicy,
    IoMode,
};

/// Begin marker for the managed block.
pub const BEGIN_MARKER: &str = "<!-- tpu-mcp:setup:begin -->";
/// End marker for the managed block.
pub const END_MARKER: &str = "<!-- tpu-mcp:setup:end -->";

/// The canonical guidance block, *not* including the begin/end markers.
///
/// Updating this string is the contract of every `tpu` release: existing
/// `--inject`ed files are upgraded automatically by re-running
/// `tpu setup --inject`.
pub fn guidance_body() -> &'static str {
    r#"## File I/O — use `tpu_*` MCP tools, never PowerShell or shell

This workspace runs the **tpu-mcp** MCP server which exposes encoding-aware
file primitives as first-class tools. Plain `Get-Content` / `Set-Content` /
`Out-File` / `>` / `cat` / `sed` round-trip files through the active code
page and silently corrupt UTF-8, UTF-16, smart quotes, em-dashes, and
box-drawing characters. Use the MCP tools instead — they detect, preserve,
and round-trip the file's native encoding and line endings safely.

**Rule:** when working in any project that has the tpu-mcp server registered,
ALWAYS prefer the `tpu_*` tools over PowerShell or shell file commands.

| MCP tool | Use it for |
|---|---|
| `tpu_read_file` | reading text files (UTF-8, UTF-16, Windows-1252, Shift-JIS, …) |
| `tpu_read_head` / `tpu_read_tail` | first/last N lines or bytes |
| `tpu_read_file_binary` | inspecting raw bytes of binary files |
| `tpu_read_file_escaped` | reading text as a single 7-bit-clean escaped line |
| `tpu_write_file` | replacing a text file's full contents |
| `tpu_append_file` | appending text to an existing file |
| `tpu_replace_in_file` | regex / fixed-string substitution (use `fixed_strings: true` for literal targets) |
| `tpu_edit_file` | targeted insert/delete/splice at known line numbers |
| `tpu_validate_file` | pre-flight assertion that a file is in the expected state |
| `tpu_count_file` | line / word / char / byte / pattern counts |
| `tpu_find` | encoding-aware grep across files and globs |
| `tpu_copy_file` | copy a file or recursively copy a tree (resilient: per-entry warnings, never aborts mid-walk by default) |
| `tpu_render_file` | populate a file from a `{{TOKEN}}` template |
| `tpu_stat_file` | verify a write actually persisted (mtime / size) |
| `tpu_setup` | (re)write this guidance block into the active `copilot-instructions.md` |

### When to use each

- **Reads** — always use `tpu_read_file`. Never use PowerShell `Get-Content`
  for code review or content inspection.
- **Edits** — prefer `tpu_replace_in_file` with `fixed_strings: true` over
  `tpu_edit_file` when the target text is unique, because line numbers can
  shift between reads. Use `tpu_edit_file` when you have just read the file
  and know exact line offsets.
- **Writes that should be guarded** — pass `validate: [{ "selector":
  "line-contains:N", "value": "..." }]` to refuse the write if the file is
  not in the expected state.
- **Globs / recursion** — `tpu_find` and `tpu_copy_file` accept glob
  patterns and tolerate inaccessible directories by emitting warning
  records (configurable via the `on_error` argument).
- **Dependency-free templating** — `tpu_render_file` substitutes
  `{{NAME}}`-style tokens. Use `\{{` to emit literal braces.

### File encoding

When you must fall back to PowerShell, never round-trip non-ASCII files
through `Get-Content` / `Set-Content` — read and write via
`[System.IO.File]::ReadAllBytes` / `WriteAllBytes` and validate with
`tools/check-encoding.ps1` afterwards.
"#
}

/// Return the full managed block (markers + body), terminated by a single LF.
pub fn full_block() -> String {
    format!("{BEGIN_MARKER}\n{}\n{END_MARKER}\n", guidance_body().trim_end())
}

/// Inject (or re-inject) the managed block into `target_file`.
///
/// On success returns `(updated, replaced)` where `updated` is whether the
/// file's content changed and `replaced` is whether an existing block was
/// replaced (vs. a fresh append).
pub fn inject(
    target_file: &Path,
    io_mode: IoMode,
) -> Result<(bool, bool), Box<dyn std::error::Error>> {
    let block = full_block();

    if !target_file.exists() {
        // Fresh file: write the block as-is via tpu's normal write path so
        // it picks up UTF-8 / LF defaults. Create the parent directory if
        // needed (e.g. ".github/").
        if let Some(parent) = target_file.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!(
                        "setup: cannot create parent {}: {e}",
                        parent.display()
                    )
                })?;
            }
        }
        crate::cmd::write::run(
            target_file,
            &block,
            OutputEncoding::Preserve,
            BomPolicy::default(),
            None,
            None,
            io_mode,
            WritePolicy::default(),
        )?;
        return Ok((true, false));
    }

    // Existing file: load LF-normalised text, splice or append the block.
    let branch = crate::open_as_branch(target_file, io_mode)?;
    let len = branch.byte_len();
    let source = Source::new(Arc::clone(&branch), SourceConfig::default())?;
    let bom_len = source.bom_len();
    let encoding = source.encoding();
    let lines_iter = source.as_lines()?;
    let view = lines_iter.view_range(bom_len as u64..len)?;
    let (cow, _) = encoding.decode_without_bom_handling(&view.bytes);
    let existing = cow.into_owned();

    let (new_text, replaced) = match (existing.find(BEGIN_MARKER), existing.find(END_MARKER)) {
        (Some(b), Some(e)) if e > b => {
            // Replace from begin..(end + END_MARKER.len()).
            let end = e + END_MARKER.len();
            let mut out = String::with_capacity(existing.len() + block.len());
            out.push_str(&existing[..b]);
            out.push_str(block.trim_end());
            out.push_str(&existing[end..]);
            (out, true)
        }
        _ => {
            // Append, ensuring exactly one blank line of separation.
            let mut out = existing.clone();
            if !out.is_empty() && !out.ends_with("\n\n") {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(&block);
            (out, false)
        }
    };

    if new_text == existing {
        return Ok((false, replaced));
    }

    crate::cmd::write::run(
        target_file,
        &new_text,
        OutputEncoding::Preserve,
        BomPolicy::default(),
        None,
        None,
        io_mode,
        WritePolicy::default(),
    )?;
    Ok((true, replaced))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_contains_markers_and_known_tools() {
        let b = full_block();
        assert!(b.contains(BEGIN_MARKER));
        assert!(b.contains(END_MARKER));
        for tool in &[
            "tpu_read_file",
            "tpu_write_file",
            "tpu_replace_in_file",
            "tpu_copy_file",
            "tpu_render_file",
            "tpu_setup",
        ] {
            assert!(b.contains(tool), "guidance must mention {tool}");
        }
    }
}
