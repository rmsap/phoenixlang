import * as path from "path";
import * as fs from "fs";
import * as https from "https";
import * as zlib from "zlib";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;

const GITHUB_REPO = "rmsap/phoenixlang";

interface PlatformInfo {
  target: string;
  archive: "tar.gz" | "zip";
  binary: string;
}

function getPlatformInfo(): PlatformInfo | undefined {
  const platform = process.platform;
  const arch = process.arch;

  if (platform === "linux" && arch === "x64") {
    return { target: "x86_64-unknown-linux-gnu", archive: "tar.gz", binary: "phoenix-lsp" };
  }
  if (platform === "linux" && arch === "arm64") {
    return { target: "aarch64-unknown-linux-gnu", archive: "tar.gz", binary: "phoenix-lsp" };
  }
  if (platform === "darwin" && arch === "x64") {
    return { target: "x86_64-apple-darwin", archive: "tar.gz", binary: "phoenix-lsp" };
  }
  if (platform === "darwin" && arch === "arm64") {
    return { target: "aarch64-apple-darwin", archive: "tar.gz", binary: "phoenix-lsp" };
  }
  if (platform === "win32" && arch === "x64") {
    return { target: "x86_64-pc-windows-msvc", archive: "zip", binary: "phoenix-lsp.exe" };
  }
  return undefined;
}

function getExpectedVersion(): string {
  const ext = vscode.extensions.getExtension(`rmsap.phoenixlang`);
  return ext?.packageJSON?.version ?? "0.0.0";
}

function getStorageDir(context: vscode.ExtensionContext): string {
  return context.globalStorageUri.fsPath;
}

function getVersionFile(storageDir: string): string {
  return path.join(storageDir, "lsp-version");
}

function isLspInstalled(storageDir: string, binary: string, expectedVersion: string): boolean {
  const binaryPath = path.join(storageDir, binary);
  const versionFile = getVersionFile(storageDir);
  if (!fs.existsSync(binaryPath) || !fs.existsSync(versionFile)) {
    return false;
  }
  const installedVersion = fs.readFileSync(versionFile, "utf-8").trim();
  return installedVersion === expectedVersion;
}

/** Follow redirects (GitHub releases redirect to S3). */
function httpsGet(url: string): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    https.get(url, { headers: { "User-Agent": "phoenixlang-vscode" } }, (res) => {
      if (res.statusCode === 301 || res.statusCode === 302) {
        const location = res.headers.location;
        if (!location) {
          return reject(new Error("Redirect with no location header"));
        }
        return httpsGet(location).then(resolve, reject);
      }
      if (res.statusCode !== 200) {
        return reject(new Error(`Download failed: HTTP ${res.statusCode}`));
      }
      const chunks: Buffer[] = [];
      res.on("data", (chunk: Buffer) => chunks.push(chunk));
      res.on("end", () => resolve(Buffer.concat(chunks)));
      res.on("error", reject);
    }).on("error", reject);
  });
}

/** Extract a single file from a .tar.gz archive. */
function extractTarGz(archive: Buffer, fileName: string): Buffer | undefined {
  const decompressed = zlib.gunzipSync(archive);
  // Minimal tar parser: 512-byte header blocks
  let offset = 0;
  while (offset + 512 <= decompressed.length) {
    const header = decompressed.subarray(offset, offset + 512);
    const name = header.subarray(0, 100).toString("utf-8").replace(/\0/g, "").trim();
    if (!name) break;

    // Parse size from octal field at offset 124, length 12
    const sizeStr = header.subarray(124, 136).toString("utf-8").replace(/\0/g, "").trim();
    const size = parseInt(sizeStr, 8) || 0;

    if (name === fileName || name === `./${fileName}`) {
      return Buffer.from(decompressed.subarray(offset + 512, offset + 512 + size));
    }

    // Advance past header + data (data is padded to 512-byte blocks)
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  return undefined;
}

/** Extract a single file from a .zip archive. */
function extractZip(archive: Buffer, fileName: string): Buffer | undefined {
  // Minimal zip parser: scan local file headers (signature 0x04034b50)
  let offset = 0;
  while (offset + 30 <= archive.length) {
    const sig = archive.readUInt32LE(offset);
    if (sig !== 0x04034b50) break;

    const compressionMethod = archive.readUInt16LE(offset + 8);
    const compressedSize = archive.readUInt32LE(offset + 18);
    const uncompressedSize = archive.readUInt32LE(offset + 22);
    const nameLen = archive.readUInt16LE(offset + 26);
    const extraLen = archive.readUInt16LE(offset + 28);
    const name = archive.subarray(offset + 30, offset + 30 + nameLen).toString("utf-8");
    const dataStart = offset + 30 + nameLen + extraLen;

    if (name === fileName || name.endsWith(`/${fileName}`)) {
      const raw = archive.subarray(dataStart, dataStart + compressedSize);
      if (compressionMethod === 0) {
        return Buffer.from(raw);
      }
      if (compressionMethod === 8) {
        return Buffer.from(zlib.inflateRawSync(raw));
      }
      return undefined;
    }

    offset = dataStart + compressedSize;
  }
  return undefined;
}

async function downloadLsp(
  context: vscode.ExtensionContext,
  platformInfo: PlatformInfo,
  version: string,
): Promise<string> {
  const storageDir = getStorageDir(context);
  fs.mkdirSync(storageDir, { recursive: true });

  const tag = `v${version}`;
  const archiveFile = `phoenix-${tag}-${platformInfo.target}.${platformInfo.archive}`;
  const url = `https://github.com/${GITHUB_REPO}/releases/download/${tag}/${archiveFile}`;

  const archive = await httpsGet(url);

  let binary: Buffer | undefined;
  if (platformInfo.archive === "tar.gz") {
    binary = extractTarGz(archive, platformInfo.binary);
  } else {
    binary = extractZip(archive, platformInfo.binary);
  }

  if (!binary) {
    throw new Error(`Could not find ${platformInfo.binary} in downloaded archive`);
  }

  const binaryPath = path.join(storageDir, platformInfo.binary);
  fs.writeFileSync(binaryPath, binary);

  // Make executable on Unix
  if (process.platform !== "win32") {
    fs.chmodSync(binaryPath, 0o755);
  }

  // Write version marker
  fs.writeFileSync(getVersionFile(storageDir), version);

  return binaryPath;
}

async function getLspPath(context: vscode.ExtensionContext): Promise<string | undefined> {
  // Priority 1: user-configured path
  const config = vscode.workspace.getConfiguration("phoenix");
  const configPath = config.get<string>("lspPath", "");
  if (configPath) {
    return configPath;
  }

  const platformInfo = getPlatformInfo();
  if (!platformInfo) {
    vscode.window.showWarningMessage(
      "Phoenix: No pre-built language server available for your platform. " +
      "Install phoenix-lsp manually and set phoenix.lspPath in settings."
    );
    return undefined;
  }

  const storageDir = getStorageDir(context);
  const version = getExpectedVersion();

  // Priority 2: already downloaded and correct version
  if (isLspInstalled(storageDir, platformInfo.binary, version)) {
    return path.join(storageDir, platformInfo.binary);
  }

  // Priority 3: prompt user to download from GitHub Releases
  const choice = await vscode.window.showInformationMessage(
    "Phoenix language server not found. Download it now for hover, autocomplete, and go-to-definition?",
    "Download",
    "Not now"
  );

  if (choice !== "Download") {
    return undefined;
  }

  return vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: "Phoenix: Downloading language server...",
      cancellable: false,
    },
    async () => {
      try {
        const binaryPath = await downloadLsp(context, platformInfo, version);
        return binaryPath;
      } catch (e: unknown) {
        const msg = e instanceof Error ? e.message : String(e);
        vscode.window.showErrorMessage(
          `Phoenix: Failed to download language server: ${msg}. ` +
          `Install phoenix-lsp manually and set phoenix.lspPath in settings.`
        );
        return undefined;
      }
    }
  );
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const serverPath = await getLspPath(context);
  if (!serverPath) {
    return;
  }

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
