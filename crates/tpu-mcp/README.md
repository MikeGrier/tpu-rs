# tpu-mcp — MCP server for encoding-safe file operations

`tpu-mcp` is a [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) server
that exposes the file-processing capabilities of `tpu` as tools callable by AI agents
such as GitHub Copilot in VS Code.

`tpu` uses [harrier](https://crates.io/crates/harrier) to detect and handle every common text encoding
(UTF-8, UTF-16LE/BE, Windows-1252, Shift-JIS) and line-ending convention (LF, CRLF, CR)
transparently, and performs all writes atomically with an automatic `.bak` backup.
Using `tpu-mcp` instead of raw PowerShell or shell commands avoids the encoding
corruption that tools like `Get-Content` or `Set-Content` routinely introduce.

---

## Prerequisites

| Requirement | Version |
|---|---|
| Rust toolchain | stable (see `rust-toolchain.toml`) |
| Cargo | ships with Rust |

---

## Build and install

> **Note:** This project is primarily developed and tested on Windows. Linux and macOS
> are not formally supported, but the Rust code itself is cross-platform; the notes
> below call out platform differences where they exist.

### Recommended — install to `~/bin` with `cargo install`

From the workspace root, run the commands for your shell:

**Windows — PowerShell**
```powershell
cargo install --path src/tools/tpu --root $HOME
cargo install --path src/tools/tpu-mcp --root $HOME
```

**Windows — Command Prompt (cmd.exe)**
```cmd
cargo install --path src/tools/tpu --root %USERPROFILE%
cargo install --path src/tools/tpu-mcp --root %USERPROFILE%
```

**Linux / macOS — bash / sh**
```sh
cargo install --path src/tools/tpu --root ~ && cargo install --path src/tools/tpu-mcp --root ~
```

This compiles both binaries in release mode and places them in `~/bin/`:

| Platform | Installed binaries |
|---|---|
| Windows | `%USERPROFILE%\bin\tpu.exe`, `%USERPROFILE%\bin\tpu-mcp.exe` |
| Linux / macOS | `~/bin/tpu`, `~/bin/tpu-mcp` |

Both binaries are required: `tpu-mcp` spawns `tpu` as a subprocess for all file
operations, and looks for it in the same directory as itself at runtime.

The VS Code configuration examples below use `${userHome}/bin/tpu-mcp`, which resolves
to the same location on all platforms. VS Code automatically appends `.exe` on Windows,
so no platform-specific config is needed.

> **Tip:** Make sure `~/bin` is on your `PATH` if you also want to run `tpu` and
> `tpu-mcp` from the terminal directly (not required for the VS Code MCP integration).

### Alternative — build only (no install)

If you prefer to keep the binaries in the workspace `target/` directory:

```sh
cargo build --release -p tpu -p tpu-mcp
```

| Platform | `tpu` binary | `tpu-mcp` binary |
|---|---|---|
| Windows | `target/release/tpu.exe` | `target/release/tpu-mcp.exe` |
| Linux / macOS | `target/release/tpu` | `target/release/tpu-mcp` |

Both binaries are required at runtime: `tpu-mcp` spawns `tpu` as a subprocess and
looks for it in the same directory as itself. You will need to use the full path to
`tpu-mcp` in the VS Code configuration below.

---

## VS Code / GitHub Copilot configuration

MCP servers are configured via a `.vscode/mcp.json` file in the workspace, or in the
VS Code user settings under the `mcp` key.

### Option 1 — per-workspace (`.vscode/mcp.json`)

Create or update `.vscode/mcp.json` in your project root:

```jsonc
{
  "servers": {
    "tpu": {
      "type": "stdio",
      "command": "${userHome}/bin/tpu-mcp",
      "args": []
    }
  }
}
```

`${userHome}` is resolved by VS Code to the current user's home directory on all
platforms. On Windows the binary must be named `tpu-mcp.exe` on disk, but VS Code
appends `.exe` automatically when resolving commands on Windows, so the path above
works unchanged on Windows, Linux, and macOS.

### Option 2 — user settings (`settings.json`)

Add to your VS Code user `settings.json`:

```jsonc
{
  "mcp": {
    "servers": {
      "tpu": {
        "type": "stdio",
        "command": "${userHome}/bin/tpu-mcp",
        "args": []
      }
    }
  }
}
```

After saving, restart the Copilot Chat extension or run
**Developer: Reload Window** (`Ctrl+Shift+P`).

---

## Tool reference

All tools accept absolute paths. Paths must use the OS-native separator
(backslash on Windows is fine; forward slashes also work).

| Tool | Read / Write | Description |
|---|---|---|
| `tpu_read_file` | read | Read a text file as UTF-8/LF, with optional line-range selection and line-number prefixes. Handles all common encodings and line endings. |
| `tpu_write_file` | write | Write UTF-8/LF text to a file, preserving the file's existing encoding and line endings. Backs up the original to `<file>.bak`. |
| `tpu_replace_in_file` | write | In-place regex substitution on the LF-normalised content of a file. CRLF transparent. Supports capture groups and multiline mode. |
| `tpu_edit_file` | write | Targeted in-place edits at known line numbers or byte offsets. Supports delete, insert, and splice operations with optional pre-edit validation. |
| `tpu_read_file_binary` | read | Read raw bytes as a 7-bit-clean escaped string (`\xHH` for non-printable bytes). For binary files and byte-level inspection. |
| `tpu_read_file_escaped` | read | Read a text file as a single flat escaped line. Every control character, including newlines, becomes a `\n`/`\t`/`\uXXXX` escape. Safe for shell variables and JSON fields. |
| `tpu_validate_file` | read | Assert that a specific line or byte range in a file matches an expected value. Use as a pre-flight guard before modifying a file. |
| `tpu_read_head` | read | Emit the first N lines or N bytes of a file. Encoding-aware; defaults to 10 lines. |
| `tpu_read_tail` | read | Emit the last N lines or N bytes of a file. Encoding-aware; defaults to 10 lines. |
| `tpu_count_file` | read | Count lines, words, characters, bytes, and/or regex pattern matches in a file. Returns a JSON object with the requested counts. |
| `tpu_append_file` | write | Append UTF-8/LF text to an existing file, preserving its encoding and line endings. Backs up the original to `<file>.bak`. |
| `tpu_find` | read | Search one or more files or glob patterns for lines matching a regex or fixed-string pattern. Prefer this over `Select-String`, `grep`, or `rg` to avoid encoding corruption. |

### `tpu_read_file`

```jsonc
{
  "file": "C:/project/src/main.rs",      // required
  "lines": "10-25",                       // optional: single line "N" or range "N-M" (1-based)
  "numbers": true                         // optional: prefix each line with its number
}
```

### `tpu_write_file`

```jsonc
{
  "file": "C:/project/src/main.rs",      // required
  "content": "fn main() {\n}\n"          // required: UTF-8/LF text
}
```

### `tpu_replace_in_file`

```jsonc
{
  "file": "C:/project/src/lib.rs",       // required
  "pattern": "fn old_name\\(",           // required: regex::bytes pattern
  "replacement": "fn new_name(",         // required: $0/$1/$name for groups, $$ for literal $
  "multiline": false                     // optional: make ^ and $ match LF boundaries
}
```

### `tpu_edit_file`

Make targeted in-place edits at known positions without replacing the whole file.
All positions reference the **original** file; multiple ops in one call are applied
without interference.

```jsonc
{
  "file": "C:/project/src/main.rs",      // required
  "ops": [                               // required: list of edit operations
    { "op": "delete",  "range": "42" },                             // delete line 42
    { "op": "insert",  "offset": "10", "data": "// inserted\n" },  // insert before line 10
    { "op": "splice",  "range": "5-7",  "data": "new content\n" }  // replace lines 5-7
  ],
  "validate": [                          // optional: pre-edit guards; any failure leaves file unchanged
    { "selector": "line:1", "value": "// Copyright (c)" }
  ],
  "diff": false,                         // optional: return a unified diff of the changes
  "binary": false,                       // optional: use 0-based byte offsets instead of 1-based lines
  "line_ending": "crlf"                  // optional: override output line ending
}
```

**Op types** (text mode — `binary: false`):

| `op` | Required fields | Effect |
|---|---|---|
| `delete` | `range` | Delete lines `N` or `N-M` (1-based, inclusive) |
| `insert` | `offset`, `data` | Insert `data` before line `N` |
| `splice` | `range`, `data` | Replace lines `N-M` with `data` |

In **binary mode** (`binary: true`), `range` and `offset` are 0-based byte offsets
and `data` may be encoded using `data_format: "hex"`, `"base64"`, or `"encoded"`.

### `tpu_read_file_binary`

```jsonc
{
  "file": "C:/project/data.bin",         // required
  "bytes": "1-512"                       // optional: 1-based byte range "N" or "N-M"
}
```

Output: a string where non-printable bytes appear as `\xHH`.

### `tpu_read_file_escaped`

```jsonc
{
  "file": "C:/project/src/data.txt",     // required
  "lines": "1-10",                       // optional: 1-based line range
  "numbers": false                       // optional: prefix with line numbers
}
```

Output: a single ASCII line where `\n`, `\r`, `\t`, `\uXXXX`, and `\UXXXXXXXX`
represent non-printable characters.

### `tpu_validate_file`

```jsonc
{
  "file": "C:/project/src/main.rs",      // required
  "selector": "line:42",                 // required: see selectors below
  "value": "fn main() {",               // required: expected value
  "is_binary": false                     // optional: override binary-mode detection
}
```

**Text selectors** (default — no `is_binary: true` needed):

| Selector | Checks |
|---|---|
| `line:N` | Line N (1-based) equals `value` exactly |
| `line-contains:N` | Line N contains `value` as a substring |

**Binary selectors** (auto-detected from prefix, or set `is_binary: true`):

| Selector | Checks |
|---|---|
| `bytes:OFFSET-END` | Raw bytes `[OFFSET, END)` equal `value` decoded from hex. Note: `END` is exclusive. |
| `md5:OFFSET-END` | MD5 of `[OFFSET, END)` equals `value` (32 lowercase hex chars). Note: `END` is exclusive. |
| `crc32:OFFSET-END` | CRC32 of `[OFFSET, END)` equals `value` (8 lowercase hex chars). Note: `END` is exclusive. |

`OFFSET` and `END` are 0-based byte offsets (decimal or `0x`-prefixed hex).
`END` may be `$` or `EOF` to mean end-of-file.

---

## Why tpu instead of PowerShell/shell file commands?

| Concern | PowerShell / shell | tpu-mcp |
|---|---|---|
| Encoding | `Get-Content` defaults to system ANSI; `Set-Content` silently drops non-ANSI chars | Detects and preserves UTF-8, UTF-16LE/BE, Windows-1252, Shift-JIS |
| Line endings | `Out-File` forces CRLF on Windows; mixing LF/CRLF breaks diffs and builds | Reads any ending, writes back the original convention |
| Atomicity | Partial writes leave a corrupt file on error | Writes to a temp file, then renames; the original is always intact |
| BOM handling | Inconsistent across cmdlets | Detects and preserves (or strips) BOM as requested |
| Regex replace | `sed`/`-replace` do not normalize CRLF; patterns must handle both | Matches against an LF-normalised view; CRLF is transparent |

---

## Security / trust boundary

`tpu-mcp` is a general-purpose **local** file editor. By design it imposes **no
workspace-root confinement**: any path the host user can access can be read or
written, including absolute paths and `file://` URIs outside the current
project (the `file` argument is only scheme-stripped and percent-decoded, not
restricted). The trust boundary is the same as the user's own shell.

Because the call surface is an LLM tool, treat this the way you would treat
giving a shell to an automated agent:

- A prompt-injected agent could be steered to read or overwrite sensitive
  paths (e.g. `~/.ssh/...`, credential files). Run the server under an account
  or sandbox whose filesystem permissions are scoped to what the agent should
  touch if that matters for your threat model.
- The write-time mojibake guard is a **safety net, not a security control**: it
  can be bypassed by content containing the `encoding-check: allow-mojibake`
  marker, which an LLM can be steered to include.
- User-supplied regular expressions go through the `regex` crate, which is
  linear-time and not subject to catastrophic backtracking, so untrusted
  patterns cannot cause a regex-DoS.

---

## Git-aware line endings (opt-in)

`tpu-mcp` can detect when a file's on-disk line endings differ from what git
would materialise in the working tree for that path (per `.gitattributes`
`text`/`eol` attributes and `core.autocrlf` / `core.eol`). This is a distinct
condition from mojibake — the bytes are valid, but the endings produce noisy
diffs and "whole file changed" churn.

Detection is **opt-in per call** via a `git_root` argument (an absolute path to
the repository root; there is no upward auto-discovery):

- **Read** — pass `git_root` to `tpu_read_file`, `tpu_read_head`, or
  `tpu_read_tail`. When the file's endings differ from git's expectation, the
  response is prefixed with a single `note:` line; the unchanged content
  follows.
- **Report / repair** — call `tpu_doctor` with `git_root` to list mismatched
  files (each flagged with an `eol_mismatch` object). Pass `fix: "eol"` to
  normalise line endings only, or `fix: "all"` to also peel mojibake. `eol` and
  `all` require `git_root`; the rewrite is atomic with a `<file>.bak` backup,
  and UTF-16 files are skipped.
- **Normalise on write (off by default)** — when the server is started with the
  `tpu-mcp.normalizeLineEndings` setting enabled (forwarded as the
  `TPU_EOL_NORMALIZE` environment variable; the CLI flag is `--eol-normalize`),
  mutating tools given a `git_root` denormalise to git's expected convention
  unless an explicit `line_ending` is supplied. This is **off by default** so
  writes never silently rewrite endings without opt-in.

---

## Protocol

`tpu-mcp` implements [MCP 2024-11-05](https://spec.modelcontextprotocol.io/) over
`stdio` using newline-delimited JSON-RPC 2.0. It supports:

- `initialize` / `notifications/initialized`
- `tools/list` / `tools/call`
- `ping`
- `shutdown`

Notifications (messages without an `id` field) are handled without sending a response,
which is correct MCP behaviour.

### Tool output format

Tool call results use a **mixed format** inside the MCP `content[0].text` field.
The first line is always a JSON invocation header; subsequent lines depend on
the tool type (see table below).  Not every line is JSON — read tools and
`tpu_find` emit raw content between the header and trailer.

| Line | Content |
|---|---|
| First | `{"reason":"x-tpu-mcp-invocation","tool":"tpu_NAME","args":{...}}` — invocation echo; large text fields replaced with `"<N bytes>"` |
| Body (mutating tools — normal write) | Optional unified diff lines, then `{"status":"success","file":"...","mtime_epoch_ms":N,"size":N}` |
| Body (mutating tools — `dry_run:true`) | Optional diff lines, then `{"status":"success","changed":true\|false}` (replace only; no file written) |
| Body (mutating tools — `count:true`) | `{"status":"success","count":N}` (replace only; no file written) |
| Body (mutating tools — `append diff:true`) | Diff lines when changed, then `{"status":"success","file":"...","changed":true\|false}` |
| Body (structured tools) | `{"reason":"x-tpu-mcp-result",...}` then `{"status":"success"}` |
| Body (read tools) | Raw file content (no JSON trailer on success) — **exception**: `tpu_read_file_binary` with a `hash` arg acts like a structured tool (see next row) |
| Body (`tpu_read_file_binary` + `hash`) | `{"reason":"x-tpu-mcp-result","encoding":"bytes-base64","content":"<base64>","hashes":[...]}` then `{"status":"success"}` |
| Body (`tpu_find`) | Matching lines as plain text, then `{"status":"success","warnings":[...]}` trailer |
| On error | `{"status":"error","message":"..."}` as the final line; `isError: true` in the MCP wrapper |
