# tpu-rs
Rust language Text Processing Utility

[![CI](https://github.com/MikeGrier/tpu-rs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/MikeGrier/tpu-rs/actions/workflows/ci.yml)
[![build-extension](https://github.com/MikeGrier/tpu-rs/actions/workflows/build-extension.yml/badge.svg?branch=main)](https://github.com/MikeGrier/tpu-rs/actions/workflows/build-extension.yml)

## VS Code extension (`tpu-mcp`)

The [`tpu-mcp`](crates/tpu-mcp) crate ships as a VS Code extension that
auto-registers an MCP server for encoding-safe file I/O from GitHub
Copilot Chat.

Once published, install it from the Marketplace:

```
code --install-extension MikeGrierTools.tpu-mcp
```

(Marketplace listing goes live with the first `tpu-mcp-v*` tag; until
then, install a per-platform `.vsix` from a
[build-extension](https://github.com/MikeGrier/tpu-rs/actions/workflows/build-extension.yml)
run.)

