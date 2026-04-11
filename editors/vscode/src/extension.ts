import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export function activate(context: vscode.ExtensionContext): void {
  const config = vscode.workspace.getConfiguration("phoenix");
  const serverPath = config.get<string>("lspPath", "phoenix-lsp");

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
