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

### Capture-group `$` expansion is conditional, and regex is opt-in

The `$`-reference expansion itself happens in the `tpu` library, not here, and only
fires when `regex:true` is set AND the pattern has an explicit capturing group (see the
"Replacement Capture-Group Expansion" section of the top-level design notes).  For a
group-less pattern — the default literal search, and any regex without `( … )` — the
replacement is written literally, so a bare `$` (e.g. `$5.00`, `$HOME`, `${TOKEN}`)
survives.  This is complementary to the backslash-escape handling above:
`unescape_replacement` still runs regardless of group presence, because `\n`/`\t`/etc.
are transport ergonomics, not capture syntax.  The `tpu_replace_in_file` schema
documents both behaviours.

Regex is opt-in (`regex:true`) rather than the default because ambiguous
capture-group syntax is easy to trigger by accident: `${1}token` is a group-1
reference followed by literal text, but `$1token` is parsed as a reference to a
group *named* `1token` (almost never present), silently dropping both the
substitution and the suffix. Defaulting to literal matching means this class of
bug can only occur when an agent has deliberately opted into regex mode.

---

## `tpu_create_file` — a create-only sibling of `tpu_write_file`

Agents frequently need to create brand-new files, but the `write_file` name does not
signal that it also creates files.  Copilot therefore struggles to anticipate the right
tool for "make a new file" and sometimes falls back to shell redirection (which corrupts
encodings).  Rather than document `write_file` harder — swimming upstream against the
tool name — `tpu-mcp` exposes a dedicated `tpu_create_file` tool.

### Contract

`call_create_file` mirrors `call_write_file` but calls `tpu::cmd::create::run`, which
**fails if the target path already exists** (after recovering any stranded `<file>.bak`).
The name and the fail-on-exists contract match the "create a new file" intent exactly, so
an agent never has to reason about whether the call will create or overwrite.  To
overwrite an existing file the agent uses `tpu_write_file`.

### Parameter subset

The tool is a deliberate subset of `write_file`, limited to what makes sense for a fresh
file:

| Parameter | Purpose |
|---|---|
| `file` (required) | absolute path of the new file; must not exist |
| `content` (required) | UTF-8/LF text; CRLF/CR normalised to LF at the boundary |
| `line_ending` (optional) | force `lf`/`crlf`/`cr` (default LF for new files) |
| `git_root` (optional) | follow the repo's configured convention when the server has EOL normalisation enabled |
| `allow_mojibake` (optional) | disable the write-time mojibake guard |

`validate` and `diff` are intentionally omitted: there is nothing to validate against on a
non-existent file, and the whole content is the change so a diff is redundant.  New files
default to UTF-8/LF; non-UTF-8 output remains out of scope per the `tpu` design.  The tool
runs through the same `stamp_and_verify` write-verification path as the other mutating
tools.

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
matching `v*` (created automatically by release-please when the Release PR
is merged). The workflow's publish job declares
`environment: marketplace`, which in turn requires manual approval from
a configured reviewer. The Azure DevOps PAT (`VSCE_PAT`) and Open VSX
token (`OVSX_PAT`) are scoped to that environment and are therefore
unavailable to any other workflow run on the repo, including PRs from
forks. Combined with the publisher PAT being limited to the
`Marketplace -> Manage` scope, the worst-case blast radius of a
leaked secret is bounded to "malicious publish to the `reirGleahciM`
publisher namespace" -- it cannot pivot to any other Azure DevOps or
GitHub resource.

---

## Out-of-process I/O isolation (`--io-worker`)

### Problem

Windows Defender's minifilter has historically terminated the long-running
`tpu-mcp` process when it performs file I/O at high rates: LLVM-built
binaries doing rapid file operations match Defender's heuristics for
suspicious behaviour, and the kill takes down the active MCP session. The
file-bytes-vs-mmap mitigation (`IoMode::Buffered` in the `tpu` crate) was
necessary but not sufficient.

### Mechanism

`tpu-mcp` now spawns one child of *itself* via `--io-worker` and forwards
every `tools/call` request to it over an anonymous stdin/stdout pipe pair.
The child runs the exact same `tools::call(name, args, config)` dispatch
function the parent would use in-process, so behaviour is byte-identical
— only the address space hosting the I/O changes.

Wire format (newline-delimited JSON, one object per line):

- Request: `{"id": N, "name": "tpu_write_file", "args": {...},
  "config": {...}}` — `config` is `ServerConfig::to_wire()`.
- Response: `{"id": N, "ok": "<text>"}` or `{"id": N, "err": "<msg>"}`.

The worker connection is held in an `IoWorkerHandle` guarded by a
`Mutex<Option<IoWorker>>`. Calls serialise on the mutex, which is fine —
the MCP server is already single-threaded per stdio session.

### Fault tolerance

Worker turbulence is surfaced to the client via MCP
`notifications/message` (`warning` level) so the user sees it in the
chat UI without having to consult stderr. Each retryable event emits
one notification.

Retry budget: 1 initial attempt + 3 retries with escalating backoff
(200 ms, 500 ms, 1000 ms). The whole-document write model — every
mutating tool replaces the file in full via a temp-file swap — makes
retries idempotent: rerunning a `write_file` / `replace_in_file` /
`edit_file` / `append_file` after a worker death produces the same
final on-disk state as a single successful call, so transparent retry
is safe.

- **Spawn failure** (`current_exe()` or `Command::spawn` errors): treated
  the same as a worker death — back off, retry, fall through to
  in-process execution only after the budget is exhausted. The handle's
  `inner` stays `None` between attempts so the next loop iteration
  retries the spawn.
- **Pipe error or EOF on read** (worker killed mid-call): drop the dead
  worker, emit a `notifications/message` warning naming the attempt
  number and reason, sleep the next backoff, respawn, retry.
- **All retries exhausted**: emit a final warning and fall back to
  in-process execution so the user-visible operation still succeeds.
- **Recovered after retry**: emit a `notifications/message` warning
  noting the successful attempt number, so the user can tell the
  difference between a clean call and a noisy-but-eventually-successful
  call without scraping logs.
- **Tool returned an error** (worker ran the tool, tool failed): this is
  a normal result, *not* a worker failure. The error string is propagated
  verbatim to the MCP client as the tool's `isError: true` payload, and the
  worker is reused for the next call. No retry.

### Atomic-write window

The existing write path (`tempfile::NamedTempFile` → rename original to
`<file>.bak` → persist temp → original path) has a small window where a
crash between the two renames leaves the file at `<file>.bak` and nothing
at the original path. This was already true in-process; running the same
code in a child does not widen it. Auto-recovery in the `tpu` library
closes the user-visible failure mode: every read path (`open_as_branch`,
`read_raw_bytes`) and every mutating command (`write`, `append`, `edit`)
calls `recover_stranded_backup(path)` before touching the file, which
promotes `<path>.bak` back to `<path>` if and only if `<path>` is missing
and `<path>.bak` is present. The recovery is silent (no warning emitted),
because the operation is fully safe — the `.bak` *is* the prior contents
— and the next operation proceeds against a fully recovered file. The
chaos test suite (`tests/io_worker_chaos.rs::stranded_backup_*`) verifies
this for both read and append entry points.

### Why the same binary as the worker

A separate `tpu-io-worker` bin target would have required either
duplicating the entire `tools::call` dispatch surface in another crate or
making `tools` a library. Reusing `tpu-mcp.exe --io-worker` keeps the
build, install, packaging, and VSIX paths unchanged: there is exactly one
binary to ship, and `std::env::current_exe()` finds it for free.

### Configuration

- Default: on (`cfg!(windows)`), off elsewhere.
- Disable: `--no-io-worker` CLI flag, or `TPU_MCP_NO_IO_WORKER=1`
  environment variable. Useful when running under a debugger, on a
  machine with a Defender exclusion already in place, or while
  investigating worker-related issues.

The protocol-level integration tests in `tests/mcp_protocol.rs` exercise
the io-worker path end-to-end on Windows because they spawn the real
binary; the same suite runs in-process on non-Windows targets because the
default flips off.

