// Copyright (c) Michael Grier
import * as vscode from "vscode";
import * as path from "path";

export function activate(context: vscode.ExtensionContext) {
  const binaryName =
    process.platform === "win32" ? "tpu-mcp.exe" : "tpu-mcp";
  const binaryPath = path.join(context.extensionPath, "bin", binaryName);

  const config = vscode.workspace.getConfiguration("tpu-mcp");
  const verifyDelay = config.get<number>("verifyDelayMs", 100);

  const args: string[] = [];
  if (verifyDelay !== 100) {
    args.push(`--verify-delay-ms=${verifyDelay}`);
  }

  // Register the MCP server so VS Code's Copilot chat can discover it.
  const disposable = vscode.lm.registerTool("tpu-mcp", {
    command: binaryPath,
    args,
    transport: "stdio",
  } as any);

  context.subscriptions.push(disposable);
}

export function deactivate() {}
