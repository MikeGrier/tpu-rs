<!-- Copyright (c) 2026, Michael Grier -->

# tpu-mcp — Design Notes

## Overview

`tpu-mcp` is an MCP (Model Context Protocol) server that exposes `tpu`'s
file-processing capabilities as tools callable by AI agents such as GitHub Copilot.
It communicates over JSON-RPC 2.0 on stdio using newline-delimited messages and calls
`tpu` library functions directly (no subprocess).

---

## Replacement escape-sequence expansion

`tpu_replace_in_file` passes the `replacement` string to
`regex::bytes::Captures::expand()`, which only interprets `$N`/`$$` capture-group
syntax — it does not process backslash escape sequences.  Copilot routinely sends `\n`
in replacement strings expecting a newline, which would otherwise produce the two
literal characters `\` and `n` in the output file.

**Fix:** `unescape_replacement()` in `tools.rs` expands the following sequences before
the replacement reaches the regex engine:

| Input sequence | Expands to |
|---|---|
| `\n` | LF (0x0A) |
| `\t` | TAB (0x09) |
| `\r` | CR (0x0D) |
| `\\` | `\` |

All other `\X` sequences are passed through unchanged so that capture-group syntax
(`$1`, `$name`, `$$`) is unaffected.  After unescape, `normalize_to_lf` is applied so
any `\r` or `\r\n` introduced by the escape expansion is folded to LF before the
normalised tpu view sees it.

---

## Write-verification stamp (Windows Defender mitigation)

Windows Defender's minifilter driver can silently revert a file write after the write
syscall returns success.  This produces "mysteriously missing" changes with no error
reported to the caller.

### Mechanism

After every successful mutating operation (`tpu_write_file`, `tpu_replace_in_file`,
`tpu_edit_file`, `tpu_append_file`), `stamp_and_verify()` in `tools.rs`:

1. Sets the file's last-modified time (`mtime`) to `SystemTime::now()` truncated to
   millisecond precision, using `std::fs::File::set_times()` (stable since Rust 1.75).
2. Sleeps `verify_delay_ms` milliseconds (default 100) to give Defender's asynchronous
   minifilter time to act.
3. Reads back the file's metadata and compares the actual mtime against the stamped
   value.  A divergence of more than 10 ms is treated as evidence of Defender
   interference and causes the tool to return an error with a diagnostic message that
   names the likely cause and lists recovery steps.
4. Returns `WriteStamp { mtime_epoch_ms, size }`, which is embedded in the success
   response string so Copilot can verify without a second round-trip.

When `verify_delay_ms` is 0, the stamp-and-verify cycle is skipped; metadata is read
and returned immediately without modifying the mtime.  This is appropriate once a
Defender exclusion is in place.

### Configuration

The delay is a server-level setting, not a per-call parameter.  It is configured via a
CLI argument so that it is set once per machine in `.vscode/mcp.json`:

```json
{
    "servers": {
        "tpu-mcp": {
            "type": "stdio",
            "command": "C:\\Users\\micgrier\\bin\\tpu-mcp.exe",
            "args": ["--verify-delay-ms=0"]
        }
    }
}
```

Default: `--verify-delay-ms=100`.  The Defender situation is per-machine; Copilot
should never need to think about this on individual writes.

### `tpu_stat_file`

A lightweight read-only tool that returns `{ size, mtime_epoch_ms, created_epoch_ms,
readonly }` from `std::fs::metadata`.  Copilot can call it cheaply at any time to
verify that a prior write's mtime matches the value reported in the write response.
A mismatch confirms Defender interference.

### 10 ms tolerance

Filesystem mtime resolution varies.  NTFS stores mtime in 100-nanosecond intervals but
some virtualised or network-backed filesystems round to 1 s or 2 s.  The 10 ms
tolerance is chosen to be tight enough to catch the Defender revert case (which produces
a timestamp minutes or hours in the past) while accommodating minor rounding on common
local filesystems.  On filesystems with coarser resolution the stamp-and-verify step
may report false positives; setting `--verify-delay-ms=0` disables it for those
environments.


---

## VS Code extension distribution

crates/tpu-mcp/extension/ is a VS Code extension (TypeScript, `commonjs` /
`ES2020`) that bundles a per-platform `tpu-mcp` binary and registers it
with VS Code's MCP discovery API so Copilot Chat picks it up with no
`.vscode/mcp.json` editing required.

### Mechanism

The extension calls `vscode.lm.registerMcpServerDefinitionProvider` (stable
since VS Code 1.101) with id `tpu-mcp`. This id MUST match the entry in
`contributes.mcpServerDefinitionProviders` in `package.json`. The
provider returns a single `McpStdioServerDefinition` whose `command` is
the absolute path to the bundled binary at
`<extensionPath>/bin/tpu-mcp[.exe]` and whose `args` reflect the current
`tpu-mcp.verifyDelayMs` (always passed explicitly so the binary's
compiled-in default never matters) plus any user-supplied
`tpu-mcp.extraArgs`.

The provider also fires `onDidChangeMcpServerDefinitions` on any
`tpu-mcp.*` configuration change so VS Code re-pulls the definition with
fresh args without requiring a window reload.

### Why `McpServerDefinitionProvider` (and not `registerTool`)

`vscode.lm.registerTool` is the Language Model Tools API and runs the
tool in-process as TypeScript. It is the wrong abstraction for `tpu-mcp`,
which is a separate Rust process that speaks JSON-RPC 2.0 over stdio. The
MCP server-definition-provider API is exactly the right shape: it tells
VS Code *how to spawn* an MCP server; VS Code then handles the spawn,
restart, output capture, and tool-list refresh itself.

### Per-platform packaging

The Marketplace supports per-platform VSIXes via
`vsce package --target <vscode-target>` and `vsce publish --target
<vscode-target>`. Targets shipped from this repository:

| VS Code target  | Rust target triple              |
|-----------------|---------------------------------|
| `win32-x64`   | `x86_64-pc-windows-msvc`      |
| `win32-arm64` | `aarch64-pc-windows-msvc`     |

VS Code installs only the VSIX matching the user's machine, so end users
get a small, single-binary install with no userspace dispatching logic
required.

### Binary location and `bin/VERSION`

CI populates `extension/bin/` with the platform-appropriate binary plus
a one-line `VERSION` text file. The extension reads `bin/VERSION` to
report the bundled binary's version (used by the
`tpu-mcp: Show bundled server version` command and the `version`
field on the `McpStdioServerDefinition`). When `bin/VERSION` is
absent (e.g. local dev with a hand-copied binary), the extension's own
`package.json` version is reported instead.

The `tpu-mcp.binaryPath` setting overrides the bundled binary; this is
used during local extension development against a freshly-built
`cargo build` output without rebuilding the VSIX.

### Publish gating

Publishing is performed by a GitHub Actions workflow triggered on tags
matching `tpu-mcp-v*`. The workflow's publish job declares
`environment: marketplace`, which in turn requires manual approval from
a configured reviewer. The Azure DevOps PAT (`VSCE_PAT`) and Open VSX
token (`OVSX_PAT`) are scoped to that environment and are therefore
unavailable to any other workflow run on the repo, including PRs from
forks. Combined with the publisher PAT being limited to the
`Marketplace -> Manage` scope, the worst-case blast radius of a
leaked secret is bounded to "malicious publish to the `reirGleahciM`
publisher namespace" -- it cannot pivot to any other Azure DevOps or
GitHub resource.
