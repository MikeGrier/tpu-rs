<!-- Copyright (c) 2026, Michael Grier -->

# tpu-mcp \u2014 Active checklist

Most prior work has been moved to `COMPLETED-CHECKLIST.md`. This file
tracks the current effort: shipping `tpu-mcp` as a VS Code extension on
the public Marketplace.

---

## Distribution: VS Code extension on the Marketplace

Strategy: build per-platform VSIXes in CI, gate publishing behind a
required-reviewer GitHub environment, ship from a tag. See
`DESIGN-NOTES.md` \u00a7 *VS Code extension distribution* for the full
design discussion.

### M1 \u2014 Extension scaffold (in progress)

- [x] M1-1: Replace placeholder `package.json` metadata
      (`publisher: reirGleahciM`, real `displayName`/`description`/
      `repository`/`bugs`/`homepage`/`license`/`categories`/`keywords`).
- [x] M1-2: Bump `engines.vscode` and `@types/vscode` to `^1.101.0`
      (stable MCP server-definition-provider API).
- [x] M1-3: Declare `contributes.mcpServerDefinitionProviders`
      with id `tpu-mcp` so VS Code auto-discovers the server.
- [x] M1-4: Declare `contributes.commands` for
      `tpu-mcp.copyServerPath` and `tpu-mcp.showServerVersion`.
- [x] M1-5: Replace placeholder `extension.ts` with a real
      `vscode.lm.registerMcpServerDefinitionProvider` implementation
      that returns an `McpStdioServerDefinition` pointing at the
      bundled binary, wired to the `tpu-mcp.verifyDelayMs`,
      `tpu-mcp.binaryPath`, and `tpu-mcp.extraArgs` settings.
- [x] M1-6: Add `LICENSE` and a Marketplace-friendly `README.md`
      inside `extension/`.
- [x] M1-7: Tighten `.vscodeignore` (exclude `src/`, `*.ts`, maps,
      backups, tsconfig).
- [x] M1-8: Track an empty `extension/bin/` via `.gitkeep` so the
      bundled-binary path resolves cleanly during dev (binaries
      themselves remain `.gitignore`d).
- [x] M1-9: Add `@types/node` and `rimraf` to devDependencies; add
      `clean`, `watch`, `vscode:prepublish`, `package:win-x64`,
      `package:win-arm64` scripts; turn on `esModuleInterop` /
      `skipLibCheck` and explicit `types: ["node", "vscode"]` in
      `tsconfig.json`.
- [ ] M1-10: Smoke test (deferred \u2014 Node.js not installed on dev
      box). Run `npm install`, `npm run compile`, hand-copy a fresh
      `tpu-mcp.exe` into `extension/bin/`, then `npx vsce package` to
      confirm a valid VSIX builds. **Action item: install Node.js LTS
      before validating, or rely on M2 CI matrix to perform the
      first real build.**
- [ ] M1-11: Provide an `icon.png` (128\u00d7128, transparent
      background) under `extension/` and add `"icon": "icon.png"` to
      `package.json`. Currently omitted to avoid shipping a
      placeholder.

### M2 \u2014 CI build matrix (per-platform VSIX)

- [x] M2-1: Add `.github/workflows/build-extension.yml`. Matrix:
      `windows-latest` for `win32-x64`; `windows-latest` with
      `rustup target add aarch64-pc-windows-msvc` for `win32-arm64`.
- [x] M2-2: Build `tpu-mcp` with `--release --target <triple>` per
      matrix leg.
- [x] M2-3: Copy the resulting `tpu-mcp.exe` into
      `crates/tpu-mcp/extension/bin/` and write a one-line
      `bin/VERSION` file (binary version).
- [x] M2-4: `npm install` + `npm run compile` + `npx vsce package
      --target <vscode-target>`; upload the per-target VSIX as a
      workflow artifact on every push and PR.
- [x] M2-5: Add status badge to the top-level README.

### M3 \u2014 Publish workflow (gated by environment)

- [ ] M3-1: Operator one-time setup (manual):
      claim the `reirGleahciM` Marketplace publisher under the
      `mjg@grier.tv` Microsoft account; create an Azure DevOps PAT
      with **Marketplace \u2192 Manage** scope (90-day expiry).
- [ ] M3-2: Operator one-time setup (manual): create a GitHub
      Actions environment named `marketplace` on the repo with
      `MikeGrier` as a required reviewer; add the PAT to that
      environment as the secret `VSCE_PAT`.
- [ ] M3-3: Add `.github/workflows/publish-extension.yml` triggered
      on tags matching `tpu-mcp-v*`. Job uses
      `environment: marketplace` so the publish step blocks on the
      required reviewer's approval. **(Workflow committed; awaiting
      M3-1/M3-2 operator setup before first tag push.)**
- [ ] M3-4: Workflow downloads VSIXes built by M2 (or rebuilds
      them), runs `npx vsce publish --packagePath <vsix> --pat
      $VSCE_PAT` once per target, and creates a GitHub Release for
      the tag with the VSIXes attached. **(Implemented as rebuild
      from tag SHA; gated behind `marketplace` environment.)**
- [x] M3-5: Add a "Marketplace install" section to the extension
      README and to the top-level README.

### M4 \u2014 Open VSX mirror

- [ ] M4-1: Operator one-time setup (manual): create an Open VSX
      account, generate a publish token, add it to the
      `marketplace` GitHub environment as `OVSX_PAT`.
- [ ] M4-2: Extend the publish workflow with an `ovsx publish`
      step that runs after the Marketplace publish succeeds. Use
      the same VSIXes; same gating.
- [ ] M4-3: Document the Open VSX listing in the README.
