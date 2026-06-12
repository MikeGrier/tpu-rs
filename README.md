# tpu-rs

Encoding-aware text processing utilities for Rust, the command line, and AI
coding agents.

[![CI](https://github.com/MikeGrier/tpu-rs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/MikeGrier/tpu-rs/actions/workflows/ci.yml)
[![build-extension](https://github.com/MikeGrier/tpu-rs/actions/workflows/build-extension.yml/badge.svg?branch=main)](https://github.com/MikeGrier/tpu-rs/actions/workflows/build-extension.yml)

`tpu-rs` is a Cargo workspace that ships two cooperating tools focused on one
problem: reading and writing text files without corrupting their encoding or
line endings. The common offenders &mdash; PowerShell `Get-Content` /
`Set-Content`, shell redirection through the active code page, and naive
editors &mdash; silently mangle UTF-8, UTF-16, smart quotes, em-dashes, and
box-drawing characters. `tpu` detects the actual on-disk encoding (UTF-8,
UTF-16LE/BE, Windows-1252, Shift-JIS, &hellip;) and CRLF/LF/CR convention and
round-trips them faithfully with atomic writes. The CLI keeps the prior
contents at `<file>.bak`; the MCP server cleans up successful `.bak`
backups after verification.

## Crates in this workspace

| Crate | What it is | Published |
|---|---|---|
| [`tpu`](crates/tpu) | Encoding-aware file I/O CLI: `read`, `write`, `replace`, `edit`, `find`, `doctor`, `validate`, &hellip; | [crates.io/crates/tpu](https://crates.io/crates/tpu) |
| [`tpu-mcp`](crates/tpu-mcp) | Model Context Protocol server exposing `tpu`'s primitives as tools for GitHub Copilot Chat and other MCP clients | [crates.io/crates/tpu-mcp](https://crates.io/crates/tpu-mcp) |

The VS Code extension that bundles `tpu-mcp` and auto-registers it as an MCP
server is published to the Visual Studio Marketplace:

- [Marketplace: `MikeGrierTools.tpu-mcp`](https://marketplace.visualstudio.com/items?itemName=MikeGrierTools.tpu-mcp)

## Install

### `tpu` CLI &mdash; from crates.io

```sh
cargo install tpu
```

### `tpu-mcp` server &mdash; from crates.io

```sh
cargo install tpu-mcp
```

`tpu-mcp` spawns `tpu` as a subprocess, so install both into the same
location (or ensure `tpu` is on `PATH`).

### VS Code extension &mdash; from the Marketplace

```sh
code --install-extension MikeGrierTools.tpu-mcp
```

The extension carries its own bundled `tpu-mcp` binary and registers an MCP
server with VS Code on startup; no separate `cargo install` is required for
the Copilot Chat integration (currently published for Windows: win32-x64 and
win32-arm64).

## Why use it

- **No silent mojibake.** Mutating tools refuse to *introduce* new mojibake
  patterns, and `tpu doctor` (or `tpu_doctor` over MCP) diagnoses and
  optionally repairs existing damage.
- **Round-trips real-world encodings.** UTF-8, UTF-16LE/BE, Windows-1252,
  Shift-JIS, and CRLF/LF/CR line endings are detected and preserved.
- **Safer than shelling out.** Atomic writes, automatic `.bak` backups,
  pre-flight `validate` selectors, and walk tools that warn on
  inaccessible entries instead of aborting mid-tree.
- **First-class agent integration.** Every CLI primitive is also an MCP
  tool, so Copilot Chat can edit files without falling back to PowerShell.

## Repository layout

```
crates/
  tpu/        # encoding-aware CLI + library
  tpu-mcp/    # MCP server + VS Code extension
tools/        # repo-local helpers (e.g. check-encoding.ps1)
```

See `crates/tpu-mcp/README.md` for command reference, configuration, and integration notes,
and run `tpu --help` for the full CLI command reference.

## License

MIT &mdash; see [LICENSE](LICENSE).
