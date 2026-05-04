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

    constructor(private readonly context: vscode.ExtensionContext) {
        // Re-fire when any tpu-mcp.* setting changes so VS Code re-pulls the
        // definition with the updated args.
        const sub = vscode.workspace.onDidChangeConfiguration((e) => {
            if (e.affectsConfiguration("tpu-mcp")) {
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
            // We instead show a one-shot warning so the user knows why
            // nothing appeared in the MCP server list.
            void vscode.window.showWarningMessage(
                "tpu-mcp: bundled server binary not found. " +
                    "Reinstall the extension or set 'tpu-mcp.binaryPath'.",
            );
            return [];
        }

        const version = readBundledVersion(this.context);

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
 * Best-effort lookup of the bundled `tpu-mcp` binary's version. We read it
 * from a `bin/VERSION` text file (a single line, e.g. `0.1.0`) emitted by
 * the CI packaging step. If the file is absent (e.g. local dev), the
 * extension's own `package.json` version is returned as a fallback.
 */
function readBundledVersion(context: vscode.ExtensionContext): string {
    const versionFile = path.join(context.extensionPath, "bin", "VERSION");
    try {
        if (fs.existsSync(versionFile)) {
            const v = fs.readFileSync(versionFile, "utf8").trim();
            if (v.length > 0) {
                return v;
            }
        }
    } catch {
        // fall through to fallback
    }
    return context.extension.packageJSON.version ?? "0.0.0";
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
            const version = readBundledVersion(context);
            const binary = resolveBinaryPath(context) ?? "(not found)";
            await vscode.window.showInformationMessage(
                `tpu-mcp server version ${version} \u2014 ${binary}`,
            );
        }),
    );
}

export function deactivate(): void {
    // All disposables are managed via context.subscriptions.
}
