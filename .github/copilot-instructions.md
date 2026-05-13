# Editing files in this repository

<!-- encoding-check: allow-mojibake (this file contains literal mojibake
     examples for documentation purposes) -->

<!-- tpu-mcp:setup:begin -->
## File operations — use the `tpu-mcp` MCP tools, never PowerShell file I/O

This repository ships a **`tpu-mcp` MCP server** that exposes every common
file operation as a first-class MCP tool. The tools preserve byte-exact
content, native encodings, and line endings, and they refuse to introduce
new mojibake — guarantees that PowerShell `Set-Content` / `Out-File` /
`Get-Content` / `>` actively violate.

**Rule:** When working in this repository, ALWAYS use the `tpu_*` MCP
tools listed below instead of running file-manipulation commands in a
PowerShell or bash terminal. This applies even inside a larger workflow —
do not switch to the terminal for file I/O just because a previous step
used the terminal.

| MCP tool | Replaces |
|---|---|
| `tpu_read_file` | `Get-Content`, `cat`, `type` |
| `tpu_read_head` / `tpu_read_tail` | `Select-Object -First/-Last`, `head`, `tail` |
| `tpu_read_file_binary` | `Format-Hex`, `xxd`, `od` |
| `tpu_read_file_escaped` | inspecting whitespace / control bytes by hand |
| `tpu_write_file` | `Set-Content`, `Out-File`, `>`, `New-Item -Value` |
| `tpu_append_file` | `Add-Content`, `>>` |
| `tpu_replace_in_file` | `(Get-Content … ) -replace … \| Set-Content`, `sed -i` |
| `tpu_edit_file` | line-based splice / patch sequences |
| `tpu_copy_file` | `Copy-Item`, `cp`, `robocopy` |
| `tpu_render_file` | here-strings / heredoc-driven file generation |
| `tpu_find` | `Select-String`, `grep`, `rg` |
| `tpu_count_file` | `(Get-Content).Count`, `wc -l` |
| `tpu_stat_file` | `Get-Item`, `stat` |
| `tpu_validate_file` | `Get-FileHash` + manual encoding inspection |
| `tpu_setup` | (re)inject this guidance block into `.github/copilot-instructions.md` |

### When to use each tool

- **Reads** (`tpu_read_file`, `tpu_read_head`, `tpu_read_tail`,
  `tpu_read_file_binary`, `tpu_read_file_escaped`) — always prefer over
  `Get-Content`/`cat`. Reads decode the file's native encoding to UTF-8
  with LF line endings, regardless of source (UTF-16, Windows-1252,
  Shift-JIS, …) and surface a one-line note when they decode a file
  that is already mojibake'd.
- **Writes** (`tpu_write_file`, `tpu_append_file`) — always use these for
  any non-trivial file content. They preserve the destination file's
  existing encoding and line-ending convention, and they refuse to write
  content that would *introduce* new mojibake (use `allow_mojibake: true`
  only when intentionally writing curated mojibake fixtures).
- **Targeted edits** (`tpu_replace_in_file`, `tpu_edit_file`) — prefer
  over hand-rolled regex pipelines. `tpu_replace_in_file` interprets
  `\n` in the replacement as a real newline by default; pass
  `literal_replacement: true` for the literal two characters.
- **`tpu_copy_file`** — use for any file or directory copy. By default
  per-entry errors (unreadable directories, permission denied) emit a
  warning and the operation continues; pass `on_error: "fail"` for the
  legacy abort-on-first-error behaviour. Supports recursive directory
  copies and glob expansion.
- **`tpu_render_file`** — use to create files from `{{TOKEN}}` Mustache
  templates. Avoids the encoding pitfalls of PowerShell here-strings
  and shell heredocs. Provide either an inline `template` or a
  `template_file`; control unknown-token behaviour via `missing`
  (`error` | `empty` | `leave`).
- **`tpu_find`** — use for code/text search. Streams matches with
  optional `before`/`after` context. Pass `on_error: "fail"` to abort
  on walk errors instead of warning and continuing.
- **`tpu_setup`** — use once per workspace (and after each tpu-mcp
  upgrade) to (re)inject the canonical guidance block, delimited by
  `<!-- tpu-mcp:setup:begin -->` / `<!-- tpu-mcp:setup:end -->`, into
  `.github/copilot-instructions.md`.
<!-- tpu-mcp:setup:end -->

## Tool preference (use the first available)

1. **Editor edit tools** (the IDE's built-in edit / replace operations) —
   always safe. Use these for any normal source-file edit.
2. **`tpu-mcp` MCP server** — preferred for batch / scripted edits and for
   any edit where preserving the file's original encoding or line endings
   matters. Detect availability by listing MCP tools; if `tpu_*` tools are
   present, use them.
3. **`tpu` CLI** (`cargo run -p tpu --` or an installed `tpu`) — same
   guarantees as `tpu-mcp`, just invoked as a subprocess. Use when
   `tpu-mcp` is unavailable.
4. **PowerShell / shell redirection** (`Set-Content`, `Out-File`, `>`,
   `>>`, `Add-Content`, `[System.IO.File]::WriteAll*`) — **fallback of
   last resort.** Allowed only when *both*:
   - the file is known to be ASCII-only (no non-ASCII bytes anywhere), and
   - none of the tools above are available.

PowerShell is fine — and preferred — for read-only inspection
(`Get-ChildItem`, `Select-String`, `git status`, …) and for running build
or test commands. The restriction is specifically on *editing* files.

## Mandatory rules when the fallback is used

- Read with `[System.IO.File]::ReadAllBytes` and write back with
  `[System.IO.File]::WriteAllBytes`. **Never** round-trip through
  `Get-Content` / `Set-Content` for any file that may contain non-ASCII
  characters — those cmdlets re-encode through the active code page and
  corrupt em-dashes, smart quotes, box-drawing characters, and any
  non-ASCII text. This has historically been the single largest source of
  file corruption in this repo.
- Immediately after the edit, run `tools/check-encoding.ps1` on the
  touched file. If it reports mojibake or invalid UTF-8, revert via
  `git checkout -- <path>` and try a different tool.
- Surface the fallback in your reply: tell the user which preferred tool
  was unavailable and why you fell back, so they can install or repair it.

## Recovering from observed tool failures

If a preferred tool appears unreliable:

1. **Diagnose, don't downgrade.** Re-read the file with the editor's
   read tool to confirm the actual on-disk bytes. Tool output may be
   misleading (stale caches, or mojibake-in-mojibake-out).
   - **First-line diagnostic for suspected corruption:** run
     `tpu doctor <path>` (or `tpu doctor <dir>` to scan a tree). It
     reports per-file encoding detection and any mojibake fingerprints
     it recognises, and `--fix=peel` will repair single-layer mojibake
     in place when it can demonstrably do so. Use `--format=json` for
     machine-parseable output. Non-destructive without `--fix`.
   - Read commands (`tpu read` / `head` / `tail` / `readex`) emit a
     one-line `note:` to stderr when they decode a mojibake'd file;
     reads themselves are never blocked. Pass `--no-mojibake-warning`
     (or set `TPU_NO_MOJIBAKE_WARNING=1`) to suppress the note when
     working on known-corrupt corpora.
   - Mutating commands (`tpu write` / `replace` / `edit` / `append`)
     and the equivalent MCP tools refuse to write content that would
     *introduce* new mojibake patterns relative to the prior file.
     Pre-existing damage is preserved (you are never punished for
     damage you did not cause). Pass `--allow-mojibake` (CLI) or
     `"allow_mojibake": true` (MCP) only when intentionally writing
     curated mojibake fixtures.
2. **Check known-fixed issues** before concluding the tool is broken:
   - `tpu replace` interprets `\n` in the replacement string as a real
     newline by default. Pass `--literal-replacement` / `-L` if you
     actually want the two characters `\` + `n`.
   - `tpu-mcp` uses buffered I/O (not memory-mapped) to avoid Windows
     Defender heuristics that previously killed it during rapid
     successive operations.
3. Only after a reproducible bug is confirmed should you fall back — and
   even then, follow the rules above.

## Encoding sanity check

`tools/check-encoding.ps1` validates every tracked text file:

- exits non-zero on any file that is not valid UTF-8;
- exits non-zero on any file that contains characteristic mojibake
  digraphs (e.g. `Ã©`, `â€"`, `â"€`, `Â `).

Run it after fallback edits, and rely on CI to run it on every PR.
For richer per-file diagnostics or in-place repair, use
`tpu doctor <path>` (optionally with `--fix=peel`); `check-encoding.ps1`
is the cheap binary gate, `tpu doctor` is the structured tool.
