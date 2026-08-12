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

## Tell Copilot to use the tools

The extension makes the `tpu_*` tools available, but Copilot still needs to
know it should prefer them over `Get-Content` / `Set-Content` / `cat` /
shell redirection. The most reliable way to do that is a short managed
section in your repository's Copilot instruction file
(`.github/copilot-instructions.md`).

You have three ways to inject (or refresh) that section:

1. **Command Palette** \u2014 `Ctrl+Shift+P` \u2192 **tpu-mcp: Open Copilot
   setup chat**. Opens Copilot Chat with a one-shot prompt that asks the
   agent to call the `tpu_setup` MCP tool against your workspace's
   `.github/copilot-instructions.md`. Review the diff Copilot proposes,
   accept, and commit.
2. **Extension settings page** \u2014 search settings for **tpu-mcp**; the
   top section (titled **tpu-mcp**) contains a single **Getting started**
   entry with a clickable **Open Copilot setup chat** link that does the
   same thing. The remaining technical settings appear in a second
   **tpu-mcp** section below.
3. **From the CLI** \u2014 if you have the `tpu` binary on your `PATH`,
   `tpu setup --inject` writes the block directly without going through
   Copilot. Useful in scripted setup.

The injected block is delimited by
`<!-- tpu-mcp:setup:begin -->` / `<!-- tpu-mcp:setup:end -->` markers, so
re-running setup is idempotent: it replaces the managed section in place
and leaves any surrounding content untouched.

Why is this step necessary? MCP tool descriptions are only visible to
Copilot after it has already chosen how to carry out a task. A repository
instruction file is loaded *before* that decision, so it reliably
intercepts the choice before Copilot reaches for a terminal. Without it
Copilot may use `tpu_*` correctly in isolation but fall back to PowerShell
in the middle of a multi-step workflow, silently corrupting non-ASCII
files.

## Settings

| Setting | Default | Purpose |
|---|---|---|
| `tpu-mcp.verifyDelayMs` | `100` | Delay (ms) after a write before re-reading file metadata, to detect Windows Defender minifilter reverts. Set to `0` once a Defender exclusion is in place. |
| `tpu-mcp.binaryPath` | `""` | Override the bundled binary path (developer use, against a locally-built `tpu-mcp`). |
| `tpu-mcp.extraArgs` | `[]` | Extra CLI args appended to the server invocation. |
| `tpu-mcp.errorMode` | `continue` | Default error policy for `tpu_find` / `tpu_copy_file` when an entry can't be read. Per-call `on_error` overrides. |
| `tpu-mcp.progressDetail` | `each-file` | How much walk-error detail tree-walking tools include in their results. |

## Commands

- **tpu-mcp: Open Copilot setup chat** \u2014 opens Copilot Chat with a
  prompt that asks the agent to run the `tpu_setup` MCP tool and inject
  the managed guidance block into `.github/copilot-instructions.md`.
- **tpu-mcp: Copy bundled server binary path** \u2014 puts the absolute
  path on the clipboard, useful for external MCP clients.
- **tpu-mcp: Show bundled server version** \u2014 displays the version of
  the bundled `tpu-mcp` binary.

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

## Bundled binary version + version-check directive

Each release of this extension bundles a specific `tpu-mcp` binary; the
**tpu-mcp: Show bundled server version** command reports which version
that is.  The bundled binary reports its own version in every response
via a `tpu_version` field on the first NDJSON line (the invocation
header), and the guidance block injected by `tpu setup` records the
version of `tpu` that wrote it as an HTML comment
(`<!-- tpu-mcp:setup:version=X.Y.Z -->`) on its first line.  Copilot is
instructed to compare the two before mutating any file and to stop and
report a mismatch.

If your Copilot sessions start reporting version drift, the fix is one
command:

- **Bundled binary older than the guidance** -- reinstall this
  extension (`code --install-extension MikeGrierTools.tpu-mcp`) and
  reload the window.  This is the common case: a defect the user reads
  about as "fixed" is still present in the running binary because the
  VSIX was not upgraded.
- **Bundled binary newer than the guidance** -- re-run `tpu setup
  --inject <path-to-copilot-instructions.md>` (or use the **Open Copilot
  setup chat** command) so the guidance refreshes against the newer
  binary.

## License

MIT -- see [LICENSE](LICENSE).
