# Editing files in this repository

<!-- encoding-check: allow-mojibake (this file contains literal mojibake
     examples for documentation purposes) -->

<!-- tpu-mcp:setup:begin -->
## File I/O — use `tpu_*` MCP tools, never PowerShell or shell

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
| `tpu_find` | encoding-aware grep across files and globs (pass `glob` to filter a directory walk, e.g. `path: "DIR", glob: "**/*.ndjson"`) |
| `tpu_copy_file` | copy a file or recursively copy a tree (resilient: per-entry warnings, never aborts mid-walk by default) |
| `tpu_render_file` | populate a file from a `{{TOKEN}}` template |
| `tpu_stat_file` | verify a write actually persisted (mtime / size) |
| `tpu_doctor` | scan files/dirs/globs for mojibake or encoding damage; optionally repair with `fix: "peel"` |
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
  records (configurable via the `on_error` argument). To search a directory
  tree with `tpu_find`, pass the directory as `path` and the filename
  pattern as `glob` (e.g. `path: "q:/src/foo/.scratch", glob: "**/*.ndjson"`)
  — this is the `find DIR -name PAT` shape and is the only way to recurse
  into an absolute directory.
- **Dependency-free templating** — `tpu_render_file` substitutes
  `{{NAME}}`-style tokens. Use `\{{` to emit literal braces.

### When a file looks corrupted (mojibake)

Symptoms: `Ã©` where `é` should be, `â€"` where `—` should be, `â"€` instead
of `─`, stray `Â ` before numbers, `ð\u009f...` blobs instead of emoji.
This is *mojibake* — text that was decoded in the wrong encoding and then
re-encoded as UTF-8. It is almost always caused by a non-tpu writer round-
tripping the file through the OS code page (PowerShell `Get-Content` /
`Set-Content` / `Out-File` / `>` / `Add-Content`, a misconfigured editor,
a generator that assumed ASCII).

Workflow:

1. **Diagnose**: call `tpu_doctor` with the suspect file (or the
   surrounding directory / glob). It returns a JSON report listing every
   flagged file, its detected encoding, per-pattern match counts, exact
   line/column locations, and whether a one-layer "peel" repair would
   strictly improve the file (`peel_suggested: true`).
2. **Identify the offender**: when a file is corrupted in a git repo, run
   `git log -p -- <file>` (or `git blame -- <file>`) to find the
   introducing commit. The commit reveals which tool wrote the damage so
   you can stop the leak at the source rather than only repairing
   downstream.
3. **Repair (conservative)**: call `tpu_doctor` again with
   `fix: "peel"`. Only files whose peel produces *strictly fewer* mojibake
   matches are rewritten; the prior content is preserved at `<file>.bak`.
   Re-run `tpu_doctor` after the repair to confirm the report is clean.
4. **Don't paper over it**: if a file legitimately contains mojibake
   digraphs (test fixtures, regex sources, documentation about mojibake),
   add the line `encoding-check: allow-mojibake` (typically inside a
   comment) — `tpu_doctor` and the write-time guard will treat it as
   clean.

The write-time guard in `tpu_write_file` / `tpu_append_file` /
`tpu_replace_in_file` / `tpu_edit_file` already refuses to *introduce* new
mojibake (pre-existing damage passes through). If you genuinely intend to
write curated mojibake fixtures, pass `allow_mojibake: true`.

### File encoding

When you must fall back to PowerShell, never round-trip non-ASCII files
through `Get-Content` / `Set-Content` — read and write via
`[System.IO.File]::ReadAllBytes` / `WriteAllBytes` and validate with
`tools/check-encoding.ps1` afterwards.
<!-- tpu-mcp:setup:end -->

<!-- cargo-mcp:setup:begin -->
## Cargo commands — use `cargo_*` MCP tools, never the terminal

This workspace runs the **cargo-mcp** MCP server. When building, testing,
linting, or otherwise driving Cargo in this Rust workspace, ALWAYS prefer
the `cargo_*` MCP tools over running `cargo` in a PowerShell or bash
terminal. This holds even mid-workflow — don't switch back to the terminal
for cargo just because a previous step used it.

| MCP tool | Replaces |
|---|---|
| `cargo_metadata` | `cargo metadata` |
| `cargo_check` | `cargo check` |
| `cargo_build` | `cargo build` |
| `cargo_test` | `cargo test` |
| `cargo_clippy` | `cargo clippy` |
| `cargo_fmt_check` | `cargo fmt --check` |
| `cargo_fmt` | `cargo fmt` |
| `cargo_tree` | `cargo tree` |
| `cargo_doc` | `cargo doc` |
| `cargo_clean` | `cargo clean` |
| `cargo_update` | `cargo update` |
| `cargo_fix` | `cargo fix` |
| `cargo_add` | `cargo add` |
| `cargo_remove` | `cargo remove` |
| `cargo_publish` | `cargo publish` |
| `cargo_nextest_run` | `cargo nextest run` (requires cargo-nextest) |
| `cargo_nextest_list` | `cargo nextest list` (requires cargo-nextest) |
| `cargo_setup` / `cargo_diagnostic` | *(no terminal equivalent)* |

Always pass `working_dir` set to the absolute path of your local checkout of
this workspace's root — the default is the cargo-mcp server's own working
directory and will usually fail to resolve the manifest or toolchain. This
path is machine- and OS-specific (e.g. `c:\GitHub\tpu-rs` on a Windows
checkout, `/home/you/tpu-rs` on Linux/macOS) — do not hardcode any single
literal path from this file; use the actual root of the checkout you are
working in.

### Boolean arguments

Boolean flags (`all_targets`, `release`, `workspace`, `lib`, `tests`, …)
take a JSON boolean (`true` / `false`). If a CLI flag you expected is
missing from the echoed `x-cargo-mcp-invocation` argv, you probably sent
the boolean in an unrecognised shape — check for a `warning` notification.

### `cargo_test` timeouts

`timeout_secs` is a hard wall-clock cap on the test **execution** phase
(armed after build/link finishes, so slow builds never trip it). Default
via the VS Code extension is 30 s; pass `timeout_secs: 0` to disable for a
slow or polling suite. In `test_filter` mode, `per_test_timeout_secs`
guards against a single hung test (idle watchdog in batched mode, hard cap
in per-test mode); a 30 s fallback keeps hung-test protection always on.

### Redirecting large output (`output_path`)

`cargo_check`, `cargo_build`, `cargo_test`, `cargo_clippy`, and `cargo_doc`
accept `output_path` (relative, parent must exist) to write the full NDJSON
transcript to a file and return only a compact summary. Use it instead of
piping to a temp file when the full output would bloat context; read the
summary first, then open the file only if it shows failures.

### Per-call env (`env`)

Every cargo-spawning tool accepts an `env` object to set/unset env vars for
that one call (e.g. `{ "env": { "RUSTFLAGS": "-C debuginfo=2" } }`). Use it
for one-shot debug knobs instead of shelling out; don't use it for
permanent config (put that in `Cargo.toml` / `.cargo/config.toml`) or
secrets.

### Optional: cargo-nextest

`cargo-nextest` is not currently installed. `cargo_test` remains the
canonical tool and is the ONLY way to run doctests. If nextest is installed
later (`cargo install cargo-nextest --locked` or
`cargo binstall cargo-nextest`), prefer `cargo_nextest_run` for per-test
process isolation, built-in retries, and filter expressions.
<!-- cargo-mcp:setup:end -->

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

### ⚠️ `tpu_replace_in_file` replacement-string escaping

`tpu_replace_in_file` **always expands** `\n`, `\r`, `\t`, and `\\` in the
`replacement` parameter to real newline / CR / tab / backslash before writing.
The `tpu replace` CLI does the same by default; pass `--literal-replacement`
(`-L`) to keep backslashes verbatim. This is intentional and documented
behaviour, but
it makes the tool **unsuitable for writing source-code escape sequences** like
`"\n"`, `"\t"` in Rust, C, Python, or JSON string literals — those sequences
become real control characters in the file.

**Rule**: when the replacement text must contain a literal `\n`, `\t`, `\r`,
or `\\` (e.g. a Rust string literal, a JSON value, a regex), use the
**VS Code editor `replace_string_in_file`** tool instead, which treats
`newString` as verbatim text with no secondary escape expansion.

To write a literal backslash-n via `tpu_replace_in_file` anyway, quadruple-
escape in JSON: `"\\\\n"` → post-JSON `\\n` → tool expands `\\` → `\` and
passes `n` through → file gets `\n`.  This is error-prone; prefer the editor
tool for any replacement that contains source-level escape sequences.

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

## Responding to PR review comments

When addressing review comments on a pull request (Copilot reviewer or
human), **reply on each individual review thread**, not only with a
summary comment on the PR conversation.

Why: as a PR accumulates more rounds of review, a single summary comment
makes it impossible to tell from the GitHub UI which inline threads are
old vs. new, or which have been addressed. A per-thread reply leaves an
"author replied" marker on each line where the reviewer raised an issue.

Procedure:

1. Fetch the inline review comments and their IDs:
   ```pwsh
   gh api repos/<owner>/<repo>/pulls/<num>/comments --jq '.[] | {id, path, line, user: .user.login, body: (.body | .[0:80])}'
   ```
2. For each comment, post a reply to that specific thread:
   ```pwsh
   gh api -X POST "repos/<owner>/<repo>/pulls/<num>/comments/<comment-id>/replies" -F body=@reply.md
   ```
3. Keep each reply short and concrete: name the commit SHA that
   addressed it and (when small) quote the new behaviour. If you
   intentionally chose not to act on a comment, say so on that thread.
4. A summary comment on the PR conversation is fine *in addition*, but
   never *instead*.

The VS Code GitHub Pull Request extension's `resolveReviewThread` tool
often reports `canResolve: false` for Copilot-authored threads; in that
case post the per-thread reply via `gh api` as above and let the human
maintainer resolve.
