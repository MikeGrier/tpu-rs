# Editing files in this repository

<!-- encoding-check: allow-mojibake (this file contains literal mojibake
     examples for documentation purposes) -->

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
