import * as path from "path";
import * as fs from "fs";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

function getBundledServerPath(context: vscode.ExtensionContext): string | undefined {
  const ext = process.platform === "win32" ? ".exe" : "";
  const bundled = path.join(context.extensionPath, "server", `phoenix-lsp${ext}`);
  if (fs.existsSync(bundled)) {
    return bundled;
  }
  return undefined;
}

export function activate(context: vscode.ExtensionContext): void {
  const config = vscode.workspace.getConfiguration("phoenix");
  const configPath = config.get<string>("lspPath", "");

  // Priority: user config > bundled binary > PATH fallback
  const serverPath = configPath || getBundledServerPath(context) || "phoenix-lsp";

  const serverOptions: ServerOptions = {
    command: serverPath,
    args: [],
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "phoenix" }],
  };

  client = new LanguageClient(
    "phoenix",
    "Phoenix Language Server",
    serverOptions,
    clientOptions
  );

  client.start();
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}
