// Copyright (c) 2026, Michael Grier
//
// tpu-mcp VS Code extension entry point.
//
// Registers the bundled `tpu-mcp` binary as an MCP server so that Copilot
// chat (and any other VS Code MCP consumer) discovers it automatically with
// no `.vscode/mcp.json` editing required.
//
// Architecture: Option B from DESIGN-NOTES.md ("Auto-register via
// contributes.mcpServerDefinitionProviders"). The provider id declared in
// `package.json` (`tpu-mcp`) MUST match the id passed to
// `vscode.lm.registerMcpServerDefinitionProvider`.

import * as vscode from "vscode";
import * as fs from "fs";
import * as path from "path";

const PROVIDER_ID = "tpu-mcp";
const SERVER_LABEL = "tpu-mcp";

/**
 * Resolve the path to the `tpu-mcp` binary that should be spawned.
 *
 * Resolution order:
 *   1. The `tpu-mcp.binaryPath` user/workspace setting (if non-empty and the
 *      file exists). Intended for developers running against a locally-built
 *      `tpu-mcp`.
 *   2. The platform-appropriate binary bundled inside the extension at
 *      `<extensionPath>/bin/tpu-mcp[.exe]`.
 *
 * Returns `undefined` if no usable binary can be located. Callers should
 * surface a clear error to the user in that case.
 */
function resolveBinaryPath(context: vscode.ExtensionContext): string | undefined {
    const config = vscode.workspace.getConfiguration("tpu-mcp");
    const override = (config.get<string>("binaryPath") ?? "").trim();
    if (override.length > 0) {
        if (fs.existsSync(override)) {
            return override;
        }
        // Fall through to bundled binary; the override pointed at a missing
        // file. We log so the user can see what happened.
        console.warn(
            `[tpu-mcp] tpu-mcp.binaryPath = ${override} does not exist; ` +
                "falling back to bundled binary.",
        );
    }

    const binaryName = process.platform === "win32" ? "tpu-mcp.exe" : "tpu-mcp";
    const bundled = path.join(context.extensionPath, "bin", binaryName);
    if (fs.existsSync(bundled)) {
        return bundled;
    }
    return undefined;
}

/**
 * Build the argument vector for spawning `tpu-mcp` based on current settings.
 *
 * `--verify-delay-ms` is always supplied so the spawned process inherits the
 * setting deterministically (no reliance on the binary's compiled-in default).
 */
function buildArgs(): string[] {
    const config = vscode.workspace.getConfiguration("tpu-mcp");
    const verifyDelayMs = config.get<number>("verifyDelayMs", 100);
    const extraArgs = config.get<string[]>("extraArgs", []) ?? [];

    const args: string[] = [`--verify-delay-ms=${verifyDelayMs}`];
    for (const a of extraArgs) {
        if (typeof a === "string" && a.length > 0) {
            args.push(a);
        }
    }
    return args;
}

class TpuMcpServerProvider
    implements vscode.McpServerDefinitionProvider<vscode.McpStdioServerDefinition>
{
    private readonly _onDidChange = new vscode.EventEmitter<void>();
    public readonly onDidChangeMcpServerDefinitions = this._onDidChange.event;

    /**
     * Tracks whether we have already shown the "binary not found" toast
     * during this VS Code session. `provideMcpServerDefinitions` may be
     * invoked repeatedly (e.g. on configuration changes or client refresh),
     * and we do not want to spam a modal warning on every call.
     */
    private missingBinaryWarned = false;

    constructor(private readonly context: vscode.ExtensionContext) {
        // Re-fire when any tpu-mcp.* setting changes so VS Code re-pulls the
        // definition with the updated args.
        const sub = vscode.workspace.onDidChangeConfiguration((e) => {
            if (e.affectsConfiguration("tpu-mcp")) {
                // A configuration change may have supplied a valid
                // `binaryPath`; allow the warning to fire again if the
                // binary is still missing on the next probe.
                this.missingBinaryWarned = false;
                this._onDidChange.fire();
            }
        });
        context.subscriptions.push(sub, this._onDidChange);
    }

    public provideMcpServerDefinitions(
        _token: vscode.CancellationToken,
    ): vscode.ProviderResult<vscode.McpStdioServerDefinition[]> {
        const binary = resolveBinaryPath(this.context);
        if (binary === undefined) {
            // Returning [] is the documented way to say "no servers right
            // now"; surfacing an error here would block extension activation.
            // Show the warning only once per session (reset on config change)
            // so that repeated provider invocations do not spam the user.
            if (!this.missingBinaryWarned) {
                this.missingBinaryWarned = true;
                void vscode.window.showWarningMessage(
                    "tpu-mcp: bundled server binary not found. " +
                        "Reinstall the extension or set 'tpu-mcp.binaryPath'.",
                );
            }
            return [];
        }

        const version = readBinaryVersion(this.context, binary);

        return [
            new vscode.McpStdioServerDefinition(
                SERVER_LABEL,
                binary,
                buildArgs(),
                /* env */ {},
                version,
            ),
        ];
    }

    public resolveMcpServerDefinition(
        server: vscode.McpStdioServerDefinition,
        _token: vscode.CancellationToken,
    ): vscode.ProviderResult<vscode.McpStdioServerDefinition> {
        // No interactive resolution required (no auth, no prompts). Return
        // the definition unchanged.
        return server;
    }
}

/**
 * Resolve the version string to advertise for `binary`.
 *
 * Resolution order, mirroring [`resolveBinaryPath`]:
 *   1. If `binary` lives inside the extension's bundled `bin/` directory,
 *      read `<extensionPath>/bin/VERSION` (written by CI). Falling back to
 *      the extension's own `package.json` version when the file is absent
 *      (local dev with a hand-copied binary) is reasonable, because in that
 *      case the binary and the extension are expected to track together.
 *   2. Otherwise the user has overridden `tpu-mcp.binaryPath`. In that case
 *      we cannot trust the bundled `VERSION` file \u2014 it describes a
 *      different binary. Look for a sibling `VERSION` file next to the
 *      override binary, and otherwise label the version as `override` so
 *      that consumers (and the `Show server version` command) do not
 *      misreport the spawned binary's identity.
 */
function readBinaryVersion(context: vscode.ExtensionContext, binary: string): string {
    const bundledDir = path.join(context.extensionPath, "bin");
    const isBundled =
        path.normalize(path.dirname(binary)).toLowerCase() ===
        path.normalize(bundledDir).toLowerCase();

    if (isBundled) {
        const v = readVersionFile(path.join(bundledDir, "VERSION"));
        if (v !== undefined) {
            return v;
        }
        return context.extension.packageJSON.version ?? "0.0.0";
    }

    // Override binary: trust only a sibling VERSION file, never the
    // bundled one (which would lie about the actually-spawned binary).
    const sibling = readVersionFile(path.join(path.dirname(binary), "VERSION"));
    if (sibling !== undefined) {
        return `${sibling} (override)`;
    }
    return "override";
}

/**
 * Read a single-line VERSION text file. Returns `undefined` if the file is
 * absent, unreadable, or empty after trimming.
 */
function readVersionFile(versionFile: string): string | undefined {
    try {
        if (fs.existsSync(versionFile)) {
            const v = fs.readFileSync(versionFile, "utf8").trim();
            if (v.length > 0) {
                return v;
            }
        }
    } catch {
        // fall through
    }
    return undefined;
}

export function activate(context: vscode.ExtensionContext): void {
    const provider = new TpuMcpServerProvider(context);
    context.subscriptions.push(
        vscode.lm.registerMcpServerDefinitionProvider(PROVIDER_ID, provider),
    );

    context.subscriptions.push(
        vscode.commands.registerCommand("tpu-mcp.copyServerPath", async () => {
            const binary = resolveBinaryPath(context);
            if (binary === undefined) {
                await vscode.window.showErrorMessage(
                    "tpu-mcp: bundled server binary not found.",
                );
                return;
            }
            await vscode.env.clipboard.writeText(binary);
            await vscode.window.showInformationMessage(
                `tpu-mcp: copied server path to clipboard: ${binary}`,
            );
        }),
    );

    context.subscriptions.push(
        vscode.commands.registerCommand("tpu-mcp.showServerVersion", async () => {
            const binary = resolveBinaryPath(context);
            if (binary === undefined) {
                await vscode.window.showInformationMessage(
                    "tpu-mcp server: binary not found",
                );
                return;
            }
            const version = readBinaryVersion(context, binary);
            await vscode.window.showInformationMessage(
                `tpu-mcp server version ${version} \u2014 ${binary}`,
            );
        }),
    );
}

export function deactivate(): void {
    // All disposables are managed via context.subscriptions.
}
