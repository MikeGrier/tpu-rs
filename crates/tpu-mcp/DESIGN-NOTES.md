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
