# Editing files in this repository

<!-- encoding-check: allow-mojibake (this file contains literal mojibake
     examples for documentation purposes) -->

<!-- tpu-mcp:setup:begin -->
<!-- tpu-mcp:setup:version=4.0.1 -->

## File I/O — use `tpu_*` MCP tools, never PowerShell or shell

This workspace runs the **tpu-mcp** MCP server which exposes encoding-aware
file primitives as first-class tools. Plain `Get-Content` / `Set-Content` /
`Out-File` / `>` / `cat` / `sed` round-trip files through the active code
page and silently corrupt UTF-8, UTF-16, smart quotes, em-dashes, and
box-drawing characters. Use the MCP tools instead — they detect, preserve,
and round-trip the file's native encoding and line endings safely.

**Rule:** when working in any project that has the tpu-mcp server registered,
ALWAYS prefer the `tpu_*` tools over PowerShell or shell file commands.

### Version check (do this first)

Every `tpu_*` response begins with an `x-tpu-mcp-invocation` header line
that includes a `tpu_version` field — the version of the running `tpu-mcp`
binary answering the call. The `<!-- tpu-mcp:setup:version=X.Y.Z -->` HTML
comment on the very first line of this block records the version of `tpu`
that wrote this guidance.

**If the two versions differ, stop and report the mismatch to the user
before performing any file mutation.** Include both versions in the
message and suggest the remedy:

- **Binary older than guidance** — rebuild / reinstall the `tpu-mcp`
  extension so its bundled binary matches this guidance, then retry. This
  is the case that has historically wasted whole sessions: a defect the
  user reads about as "fixed" is still present in the running binary
  because the extension's bundled `tpu-mcp.exe` was not updated.
- **Binary newer than guidance** — re-run `tpu setup --inject
  <path-to-copilot-instructions.md>` to refresh this block against the
  newer binary, then retry. Tool descriptions, response shapes, or
  recommended workflows may have changed.

This check is cheap (one field on one line, checked once per session) and
catches version-drift bugs before they manifest as silent behavioural
differences.

| MCP tool | Use it for |
|---|---|
| `tpu_read_file` | reading text files (UTF-8, UTF-16, Windows-1252, Shift-JIS, …) |
| `tpu_read_head` / `tpu_read_tail` | first/last N lines or bytes |
| `tpu_read_file_binary` | inspecting raw bytes of binary files |
| `tpu_read_file_escaped` | reading text as a single 7-bit-clean escaped line |
| `tpu_create_file` | creating a NEW file — fails if the path already exists |
| `tpu_write_file` | replacing an existing text file's full contents |
| `tpu_append_file` | appending text to an existing file |
| `tpu_replace_in_file` | literal (default) or regex substitution — pass `regex: true` to opt into regex matching; a run that matches nothing is an error |
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
- **Line ranges must start inside the file** — a `lines` argument (on
  `tpu_read_file` or `tpu_read_file_escaped`) whose *start* is past the last
  line is an **error** that reports the file's real line count; it is not
  empty output. End bounds are still clamped, so `lines: "1-9999"` remains
  the safe way to say "from here to EOF". Re-anchor using the line count in
  the error rather than probing with another guess.
- **New files** — use `tpu_create_file`, not `tpu_write_file`. It fails when
  the path already exists, so a mistaken path is reported instead of
  silently destroying whatever was there. Reach for `tpu_write_file` only
  when you intend to replace the contents of a file you know exists.
- **Edits** — prefer `tpu_replace_in_file` (literal matching by default,
  no escaping needed) over `tpu_edit_file` when the target text is unique,
  because line numbers can shift between reads. Use `tpu_edit_file` when
  you have just read the file and know exact line offsets. Every text
  payload — `content`, `text`, `replacement`, an op's `data` — is written
  **verbatim**: backslashes are never collapsed, so no tpu tool needs
  pre-doubled escapes. (`tpu_replace_in_file` accepts an opt-in
  `expand_escapes: true` for callers that deliberately double-escape.)
- **A replace that matches nothing is an error** — `tpu_replace_in_file`
  returns `{"status":"error"}` when `pattern` matches zero times, and leaves
  the file completely untouched (mtime preserved, no `.bak`). This is
  deliberate: a silent success on a mis-anchored pattern is
  indistinguishable from a real edit. Re-read the file and re-anchor the
  pattern instead of retrying blind. Pass `allow_no_match: true` only for a
  genuinely idempotent re-run — the response then carries `count: 0` and a
  `warning`. `count: true` and `dry_run: true` are exempt (zero is a
  legitimate answer for an introspection mode), as is a `line_ending`
  override, which rewrites the file even with zero substitutions. A real
  write always reports `count`, so no follow-up `count: true` call is needed
  to confirm how many substitutions landed.
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

### Escape-sequence hazard (JSON transport)

MCP arguments travel as JSON strings. In JSON, `\n` **is** a real newline —
a literal backslash-n in the file requires `\\n` on the wire. Under-escaping
is easy for an agent to do because it "sees" the target source text, not
the JSON transport, and the damage is invisible: the string is already
decoded to a real newline *before* `tpu_write_file` / `tpu_append_file` /
`tpu_replace_in_file` / `tpu_edit_file` ever runs, so no flag on the tool
call can distinguish "intended literal `\n`" from "intended real newline" —
no server-side option can undo a JSON decode that already happened.

tpu does not add a second layer of its own: every text payload is written
verbatim, so a correctly JSON-escaped string always lands byte-for-byte.
(`tpu_replace_in_file`'s `expand_escapes: true` is the sole exception, and
it is opt-in — leave it off unless you deliberately double-escaped.) The
residual hazard is purely in getting the JSON escaping right.

**The fix**: when a payload (`content`, `pattern`, `replacement`, or an
edit op's `data`) contains backslash escapes, embedded quotes, or anything
not certain to be JSON-escaped correctly, set the matching `*_format`
argument (`content_format`, `pattern_format`, `replacement_format`, or an
op's `data_format`) to `"base64"` and send the exact bytes base64-encoded.
Base64's alphabet has no backslashes, so there is no escaping decision to
get wrong — this makes the whole class of bug impossible rather than just
less likely. `"hex"` works the same way; avoid `"encoded"` for this purpose
since it is itself a backslash-escape codec and re-introduces the hazard.

**Safety net**: `tpu_replace_in_file` also echoes a compact changed-region
preview of every small real write by default (no `diff:true` needed — see
`echo_max_lines`), so a corruption like this is visible immediately in the
same turn instead of requiring a follow-up read. This echo is cheap
regardless of file size (it never clones the whole file); pass `diff:true`
for a full old/new unified diff instead.

### Concurrent edits — don't silently clobber (`content_version` / `if_match`)

Copilot may issue several tool calls against the same file in quick
succession. A blind `tpu_write_file` (or a `tpu_edit_file` at line numbers)
whose payload was computed from an earlier read can silently overwrite an
edit that landed in between — a lost update.

Every read (`tpu_read_file`, `tpu_read_head`, `tpu_read_tail`, `tpu_read_file_escaped`)
reports a `"content_version"` on its invocation-header line (a content digest
that changes whenever the file's bytes change), and every successful write
stamp reports the new `"content_version"`. A read MAY omit the token when the
file changed while it was being read (so it can't be guaranteed to describe
the bytes returned) — treat a missing token as "re-read before relying on a
version".

When you mutate a file based on content you previously read, pass that token
as `if_match` on `tpu_write_file` / `tpu_edit_file` / `tpu_replace_in_file` /
`tpu_append_file`. If the file changed since you read it, the call is
REFUSED with `{"status":"conflict",...}` (surfaced as an MCP error,
`isError: true`) and the file is left unchanged, instead of clobbering the
other edit; then re-read, rebuild your change against the current content,
and retry with the new `content_version`.

Prefer a narrow `tpu_replace_in_file` over a full-file `tpu_write_file` when
you can: a replace operates on the file's current bytes, so it is far less
prone to lost updates in the first place.

### Tool output format

Every tool response uses a **mixed format**: a JSON invocation header,
a content-type-dependent body, and (for most tools) a JSON status trailer.
Not every line is JSON — read tools and `find` return raw content
between the header and trailer.

- **Line 1** — invocation header:
  `{"reason":"x-tpu-mcp-invocation","tool":"tpu_NAME","args":{...}}`
  Large `content`/`replacement`/`template` fields appear as `"<N bytes>"` placeholders.
- **Mutating tools** (write, replace, edit, append) — normal write:
  `{"status":"success","file":"...","mtime_epoch_ms":N,"size":N,"content_version":"..."}`
  (`content_version` is the digest of the just-written file — pass it as
  `if_match` on your next edit of this file; see "Concurrent edits" above).
  A `tpu_replace_in_file` whose `pattern` matched zero times is instead an
  error trailer naming the count, with the file left untouched — see
  "A replace that matches nothing is an error" above.
  Preview modes do not stamp the file and return a reduced trailer:
  `diff:true` adds unified diff lines before the status (full stamp still present for write/replace/edit).
  `dry_run:true` (replace only): optional diff lines, then `{"status":"success","changed":true|false}`.
  `count:true` (replace only): `{"status":"success","count":N}`.
  `append diff:true`: diff lines when changed, then `{"status":"success","file":"...","changed":true|false}`.
  `tpu_replace_in_file` on a real write additionally reports `"changed_lines":N` in the status —
  the sum, over every match, of `(old span line count) + (new text line count)`; this is a
  cheap per-match total, NOT a deduplicated count of unique file lines, so two matches on the
  same line each contribute their own share and can push N above the file's actual line count —
  and, as long as N is at most `echo_max_lines` (default 5), automatically prepends a
  compact changed-region preview (unified-diff-style hunk headers, new lines only — no
  full-file diff) even without `diff:true`; a larger change instead adds
  `"diff_omitted":true` (pass `diff:true` for a full old/new unified diff regardless of size).
- **Structured tools** (count_file, stat_file, copy_file, render_file,
  setup+target, doctor) — result line
  `{"reason":"x-tpu-mcp-result",...}` followed by `{"status":"success"}`.
- **Read tools** (tpu_read_file, tpu_read_head, tpu_read_tail, tpu_read_file_escaped) — header then raw content; no JSON trailer on success.
  The header line usually carries a `"content_version"` token for this file (see "Concurrent edits" above); pass it as `if_match` when you later edit the file. It may be absent if the file changed mid-read — re-read to get a usable token.
  **Exception** — `tpu_read_file_binary` with a non-empty `hash` arg acts like a structured tool:
  `{"reason":"x-tpu-mcp-result","encoding":"bytes-base64","content":"<base64>","hashes":[...]}` followed by `{"status":"success"}`.
  Without `hash`, `tpu_read_file_binary` returns header + 7-bit-clean escaped bytes (no trailer).
- **Find tool** (find) — header, then matching lines as plain text, then `{"status":"success","warnings":[...]}` trailer.
- **Errors** — `{"status":"error","message":"..."}` as the final line;
  `isError: true` in the MCP wrapper.

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

### When line endings disagree with git (CRLF / LF)

A separate, git-aware condition: a file's on-disk line endings can differ
from what git would materialise in the working tree for that path (per
`.gitattributes` `text`/`eol` attributes and `core.autocrlf` / `core.eol`).
This is *not* mojibake — the bytes are valid — but it produces noisy diffs
and "whole file changed" churn.

Detection is **opt-in per call** via a `git_root` argument (an absolute path
to the repository root; there is no upward auto-discovery):

1. **Detect on read**: pass `git_root` to `tpu_read_file`, `tpu_read_head`,
   or `tpu_read_tail`. When the file's endings differ from git's expectation
   the response is prefixed with a single `note:` line and the unchanged
   content follows.
2. **Report / repair with doctor**: call `tpu_doctor` with `git_root` to
   list mismatched files (each flagged with an `eol_mismatch` object). Pass
   `fix: "eol"` to normalise line endings only, or `fix: "all"` to also peel
   mojibake. `eol`/`all` require `git_root`; the rewrite is atomic with a
   `<file>.bak` backup and UTF-16 files are skipped.
3. **Normalise on write (off by default)**: when the server is started with
   line-ending normalisation enabled (the `tpu-mcp.normalizeLineEndings` VS Code
   setting, the `--eol-normalize` flag, or the `TPU_EOL_NORMALIZE` env var),
   mutating tools given a `git_root` denormalise to git's expected
   convention unless an explicit `line_ending` is supplied. This is **off by
   default** so writes never silently rewrite endings without opt-in.

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

### `tpu_replace_in_file` replacement-string escaping

`tpu_replace_in_file` writes `replacement` **verbatim** — the same contract as
`tpu_write_file`'s `content` and `tpu_append_file`'s `text`. Backslashes are
never collapsed, so source-level escape sequences (`"\n"`, `"\t"`, `r"\\."`,
`\d+`) land in the file exactly as sent. No pre-doubling is needed for any
tpu tool.

To get a real newline, put a real newline in the JSON string. Pass
`expand_escapes: true` only if you deliberately double-escaped the payload and
want the old sed-style decoding (`\n`/`\r` → LF, `\t` → TAB, `\\` → one
backslash); it cannot be combined with `replacement_format`.

Released builds before 3.0.0 applied that decoding unconditionally, which
silently collapsed `\\` to `\` — e.g. a replacement containing `r"\\."` was
written as `r"\."`, disabling the guard it was meant to add. The
`tpu_version` in the invocation header identifies released builds, but note
that a dev build of the fix reports the pre-release version, so version alone
is not decisive. If you need certainty, probe once: replace a scratch string
with `\\` and read it back — two characters means verbatim (fixed), one means
the old behaviour, in which case route through `replacement_format: "base64"`.

Note the CLI differs on purpose: `tpu replace` interprets `\n` in the
replacement by default (sed/perl/ripgrep convention); pass
`--literal-replacement` / `-L` for verbatim.

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

## Keeping the `tpu setup` guidance current (required for every behaviour change)

`guidance_body()` in [`crates/tpu/src/cmd/setup.rs`](../crates/tpu/src/cmd/setup.rs)
is the single source of truth for the managed
`<!-- tpu-mcp:setup:begin -->` block that every consuming repository injects
into its own `copilot-instructions.md`. In a *downstream* repo that block is
the only tpu documentation an agent ever reads, so guidance that lags the
binary is not a docs nit — it actively instructs agents to drive tpu the way
it used to work. The version marker at the top of the block does not catch
this: it records the release that wrote the block, not whether the block's
prose describes that release.

**Any change to observable `tpu` / `tpu-mcp` behaviour must update
`guidance_body()` in the same commit.** At minimum that includes:

- a new, renamed, or removed `tpu_*` MCP tool — add/remove its row in the
  tool table;
- a new argument that changes what a caller should do (`allow_no_match`,
  `expand_escapes`, `if_match`, `git_root`, a `*_format` channel, …);
- any change to a default, to a success/error condition, or to a response
  shape;
- every commit carrying a `BREAKING CHANGE:` footer, without exception.

Then regenerate this repo's own copy in the same commit so the two never
diverge:

```pwsh
cargo run -p tpu -- setup --inject .github/copilot-instructions.md
```

The `repo_copilot_instructions_block_matches_generated_guidance` test in
`crates/tpu/tests/copy_render_setup.rs` fails when the checked-in block does
not match the generator's output exactly (apart from CRLF->LF normalization
and the trailing newline `tpu setup` prints), so a forgotten re-inject is
caught by CI instead of by a downstream user.

Note that the block's `<!-- tpu-mcp:setup:version=X.Y.Z -->` marker embeds
`CARGO_PKG_VERSION`, so **a version bump alone makes the block stale** even
when no prose changed. Release PRs are handled automatically by the
`sync-version-artifacts` job in `.github/workflows/release-please.yml`, which
re-injects the block alongside the `Cargo.lock` refresh. If you bump the
workspace version by hand, re-inject by hand too.

Checklist before merging or cutting a release:

1. Does the diff change anything a caller can observe? If so, does it also
   touch `guidance_body()`?
2. Is the new wording written for an *agent in another repository* — the
   tool's contract and what to do differently, not this repo's internals?
3. Was `tpu setup --inject` re-run, so the version marker in
   `.github/copilot-instructions.md` matches the current workspace version?

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
