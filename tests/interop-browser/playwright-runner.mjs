// Gated real-browser DOM tier: load the page in headless Chromium (via
// playwright-core — no bundled browser; the browser is provisioned separately),
// run the program in-page, dispatch a real click, and read back the DOM. Usage:
// `node playwright-runner.mjs <buildDir>`.
//
// The build dir is served over a tiny localhost HTTP server because browsers
// block `fetch()` of a `file://` sibling (the page fetches `app.wasm` and imports
// the glue). If no browser is installed, exits 3 with a `PHOENIX_BROWSER_UNAVAILABLE`
// marker so the Rust harness can soft-skip (vs. a real assertion failure).
//
// Observation protocol matches jsdom-runner.mjs: `run: <#label>`, then on a `#btn`
// a `click: <#label>` — so a fixture's `expected.txt` pins both tiers identically.
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { extname, join, normalize, sep } from "node:path";
import { chromium } from "playwright-core";

const buildDir = process.argv[2];
if (!buildDir) {
  console.error("usage: playwright-runner.mjs <buildDir>");
  process.exit(2);
}

const CONTENT_TYPES = {
  ".html": "text/html",
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".wasm": "application/wasm",
};

const server = createServer(async (req, res) => {
  let file;
  try {
    const { pathname } = new URL(req.url, "http://localhost");
    // Decode percent-encoding *before* normalizing so an encoded `%2e%2e`
    // collapses to a real `..` and is caught by the confinement check below —
    // otherwise it would slip through `normalize` and fall to an opaque 404.
    const decoded = decodeURIComponent(pathname);
    const rel = decoded === "/" ? "/page.html" : normalize(decoded);
    // Confine to the build dir: reject any path that escapes it. Compare against
    // `root + sep` (not bare `root`) so a sibling like `<buildDir>-x` can't slip
    // past `startsWith`; `root` itself is a directory, never a served file.
    const root = join(buildDir);
    file = join(buildDir, rel);
    if (!file.startsWith(root + sep)) {
      res.writeHead(403);
      res.end("forbidden");
      return;
    }
  } catch {
    // Malformed request line / percent-encoding — a client error, not "missing".
    res.writeHead(400);
    res.end("bad request");
    return;
  }
  try {
    const body = await readFile(file);
    res.writeHead(200, {
      "content-type": CONTENT_TYPES[extname(file)] || "application/octet-stream",
    });
    res.end(body);
  } catch (e) {
    // Distinguish "no such file" from a genuine read error so an in-page fetch
    // failure isn't silently misreported as a 404.
    if (e?.code === "ENOENT" || e?.code === "EISDIR" || e?.code === "ENOTDIR") {
      res.writeHead(404);
      res.end("not found");
    } else {
      res.writeHead(500);
      res.end("internal error");
    }
  }
});
await new Promise((resolve) => server.listen(0, resolve));
const port = server.address().port;

let browser;
try {
  // `--no-sandbox` is the standard headless-CI flag (Chromium's sandbox needs
  // privileges most CI containers don't grant); the page is trusted local
  // content, so it's safe here.
  browser = await chromium.launch({ args: ["--no-sandbox"] });
} catch (e) {
  server.close();
  console.error("PHOENIX_BROWSER_UNAVAILABLE: " + (e?.message ?? e));
  process.exit(3);
}

try {
  const page = await browser.newPage();
  // Forward in-page diagnostics to stderr. Without this, an exception thrown by
  // the in-page `run()` never sets `__phoenixDone`, so the only symptom is an
  // opaque `waitForFunction` timeout with no trace of the real cause.
  page.on("pageerror", (err) => console.error(`page error: ${err.stack ?? err}`));
  page.on("console", (msg) => console.error(`page console.${msg.type()}: ${msg.text()}`));
  await page.goto(`http://localhost:${port}/page.html`);
  await page.waitForFunction("window.__phoenixDone === true");

  const out = [`run: ${await page.textContent("#label")}`];
  if (await page.$("#btn")) {
    await page.click("#btn");
    out.push(`click: ${await page.textContent("#label")}`);
  }
  process.stdout.write(out.join("\n") + "\n");
} finally {
  await browser.close();
  // Await the close so a lingering keep-alive socket can't keep the event loop
  // (and thus the node process) alive past the script.
  await new Promise((resolve) => server.close(resolve));
}
