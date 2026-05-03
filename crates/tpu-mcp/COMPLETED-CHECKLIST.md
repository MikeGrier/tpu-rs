<!-- Copyright (c) 2026, Michael Grier -->

## Moved 2025-07-25 — Direct library calls (eliminate subprocess + clap parsing)

Replaces the subprocess/response-file invocation of `tpu` with direct library
calls to `tpu::cmd::*` functions.  This eliminates the entire class of bugs
where `--`-prefixed argument values are misinterpreted as CLI options by clap.

### Milestone 1: Library infrastructure

- [x] LIB-1: Add `human_output_to(writer)` constructor to `tpu::output` so tpu-mcp can capture output into a buffer
- [x] LIB-1.1: Add `parse_line_ending()` helper to `tpu::encoding` for reuse by tpu-mcp

### Milestone 2: Rewrite tool implementations

- [x] LIB-2: Rewrite `call_read_file` — call `cmd::read::run()` directly
- [x] LIB-3: Rewrite `call_read_file_binary` — replicate binary read logic from main.rs
- [x] LIB-4: Rewrite `call_read_file_escaped` — call `cmd::readex::run()` directly
- [x] LIB-5: Rewrite `call_read_head` — call `cmd::head::run()` directly
- [x] LIB-6: Rewrite `call_read_tail` — call `cmd::tail::run()` directly
- [x] LIB-7: Rewrite `call_count_file` — call `cmd::count::run()` with buffer-backed Output
- [x] LIB-8: Rewrite `call_find` — call `cmd::find::run()` directly
- [x] LIB-9: Rewrite `call_write_file` — call `cmd::write::run()` directly
- [x] LIB-10: Rewrite `call_replace_in_file` — call `cmd::replace::run()` directly
- [x] LIB-11: Rewrite `call_edit_file` — call `cmd::edit::run()` directly
- [x] LIB-12: Rewrite `call_append_file` — call `cmd::append::run()` directly

### Milestone 3: Cleanup

- [x] LIB-13: Remove `invoke.rs` module; move `tempfile` to dev-dependencies
- [x] LIB-14: Update tpu DESIGN-NOTES.md
- [x] LIB-15: Full build and test (debug + release), fix any warnings

## Moved 2026-04-06 — Normalize incoming text to LF at MCP boundary

Copilot/MCP clients may send CRLF or mixed line endings in JSON strings.
The tpu library expects LF-only input; CRLF in input produces `\r\r\n` on
CRLF-target files or injects CRLF into LF files.  Fix: normalize all
incoming text to LF at the tpu-mcp boundary before calling tpu functions.

- [x] NL-1: Add `normalize_to_lf(s: &str) -> Cow<str>` and `normalize_bytes_to_lf` helpers to tools.rs
- [x] NL-2: Apply to `call_write_file` (`content`)
- [x] NL-3: Apply to `call_replace_in_file` (`replacement`)
- [x] NL-4: Apply to `call_edit_file` (Insert/Splice `data` in text mode only)
- [x] NL-5: Apply to `call_append_file` (`content`)
- [x] NL-6: Add unit tests (CRLF, CR, mixed, LF-only passthrough) + 10 integration tests
- [x] NL-7: Update MCP tool schema descriptions to document LF normalization
- [x] NL-8: Update tpu DESIGN-NOTES.md
- [x] NL-9: Build and test (debug + release), fix warnings

## Moved 2026-04-07 — Escape sequence expansion and write-verification stamp

Fixed literal `\n` injection in `tpu_replace_in_file` (RE-1 through RE-5): added
`unescape_replacement()` at the MCP boundary so `\n`/`\t`/`\r`/`\\` in replacement
strings expand to real characters before reaching `regex::bytes::Captures::expand()`.

Added write-verification stamp (WV-1 through WV-8): after every mutating operation
tpu-mcp sets the file mtime to a known value, waits `--verify-delay-ms` (default 100 ms)
for Windows Defender's minifilter to act, reads back metadata, and returns
`[mtime=NNN, size=NNN]` in the response. A mtime divergence >10 ms returns a diagnostic
error. Added `tpu_stat_file` read-only tool for cheap post-write verification. Added
`ServerConfig` + `parse_config()` in `main.rs` to parse `--verify-delay-ms=N` once at
startup. Updated `copilot-instructions.md` with escape and Defender guidance. Created
`DESIGN-NOTES.md`. 70 tests pass, release build clean.

- [x] RE-1: `unescape_replacement` helper
- [x] RE-2: Wire into `call_replace_in_file`
- [x] RE-3: Unit + integration tests for escape expansion
- [x] RE-4: Tool schema `replacement` description updated
- [x] RE-5: `copilot-instructions.md` escape-sequence guidance
- [x] WV-1: `ServerConfig` + `--verify-delay-ms` CLI arg
- [x] WV-1.1: `stamp_and_verify` helper
- [x] WV-2: Stamp wired into all four write call sites
- [x] WV-3/4/5: `tpu_stat_file` tool added
- [x] WV-6: WV integration tests (WV-IT-1 through WV-IT-6)
- [x] WV-7: `copilot-instructions.md` Defender guidance + `--verify-delay-ms=0` snippet
- [x] WV-8: `DESIGN-NOTES.md` created
- [x] FIN-1: `cargo test -p tpu-mcp` — 70 passed
- [x] FIN-2: `cargo build --release -p tpu-mcp` — clean
