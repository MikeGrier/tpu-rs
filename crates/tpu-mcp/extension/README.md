# tpu-mcp -- encoding-safe file I/O for Copilot

This extension bundles the [`tpu-mcp`](https://github.com/MikeGrier/tpu-rs)
MCP (Model Context Protocol) server and registers it with VS Code so that
GitHub Copilot Chat (and any other MCP consumer) can use it for
encoding-aware file operations: read, write, replace, search, validate,
edit, and more.

`tpu-mcp` correctly handles UTF-8, UTF-16LE/BE, Windows-1252, and Shift-JIS
files; preserves the file's original line endings (CRLF / LF / CR); writes
atomically with a `.bak` backup; and refuses to introduce mojibake.

Use it in place of `Get-Content` / `Set-Content` / `cat` / shell
redirection from Copilot to avoid encoding corruption.

## Installation

Install from the VS Code Marketplace:

```
code --install-extension MikeGrierTools.tpu-mcp
```

or search for **tpu-mcp** in the Extensions view.

The extension auto-registers itself as an MCP server on startup; no
`.vscode/mcp.json` editing is required.

After install, reload the window (Command Palette \u2192 **Developer: Reload
Window**) and the `tpu-mcp` server will appear under
**MCP: List Servers**.

## Settings

| Setting | Default | Purpose |
|---|---|---|
| `tpu-mcp.verifyDelayMs` | `100` | Delay (ms) after a write before re-reading file metadata, to detect Windows Defender minifilter reverts. Set to `0` once a Defender exclusion is in place. |
| `tpu-mcp.binaryPath` | `""` | Override the bundled binary path (developer use, against a locally-built `tpu-mcp`). |
| `tpu-mcp.extraArgs` | `[]` | Extra CLI args appended to the server invocation. |

## Commands

- **tpu-mcp: Copy bundled server binary path** -- puts the absolute path
  on the clipboard, useful for VS proper / external clients.
- **tpu-mcp: Show bundled server version** -- displays the version of the
  bundled `tpu-mcp` binary.

## Tools exposed to Copilot

The full list and JSON schemas are documented in the
[tpu-mcp README](https://github.com/MikeGrier/tpu-rs/tree/main/crates/tpu-mcp#tool-reference).
A short summary:

| Tool | Description |
|---|---|
| `tpu_read_file`, `tpu_read_head`, `tpu_read_tail` | Read text with optional line-range / line numbers. |
| `tpu_read_file_binary`, `tpu_read_file_escaped` | Binary or escaped-text reads. |
| `tpu_write_file`, `tpu_append_file` | Encoding-preserving writes. |
| `tpu_replace_in_file`, `tpu_edit_file` | Regex and targeted edits, CRLF-transparent. |
| `tpu_find`, `tpu_count_file`, `tpu_validate_file` | Search, counting, pre-edit validation. |
| `tpu_stat_file` | Cheap metadata stat for write verification. |

## License

MIT -- see [LICENSE](LICENSE).
