// Always-on DOM smoke (no real browser): load the generated glue against a jsdom
// `document` and exercise DOM-mutating externs + a closure-registered event
// handler. Usage: `node jsdom-runner.mjs <buildDir>`, where <buildDir> holds the
// built `app.wasm` + `app.js` glue and the fixture's `host.mjs` + `page.html`.
//
// The glue runs in the Node context (TextDecoder / crypto / WebAssembly /
// FinalizationRegistry are Node globals); only `document` comes from jsdom, passed
// to the host via `ctx`. jsdom loads the page with `runScripts: "outside-only"`,
// so the page's in-browser module script is ignored here and the glue is driven
// from Node — the same `host.mjs` serves this tier and the real-browser tier.
//
// Observation protocol (shared with playwright-runner.mjs): after `run()`, emit
// `run: <#label textContent>`; if a `#btn` exists, dispatch a click and emit
// `click: <#label textContent>`. The fixture's `expected.txt` pins those lines.
import { JSDOM } from "jsdom";
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const buildDir = process.argv[2];
if (!buildDir) {
  console.error("usage: jsdom-runner.mjs <buildDir>");
  process.exit(2);
}

const html = await readFile(`${buildDir}/page.html`, "utf8");
const dom = new JSDOM(html, { runScripts: "outside-only" });
const { document } = dom.window;

const { instantiate } = await import(pathToFileURL(`${buildDir}/app.js`).href);
const { host } = await import(pathToFileURL(`${buildDir}/host.mjs`).href);
const wasm = await readFile(`${buildDir}/app.wasm`);

const { run } = await instantiate({
  wasm,
  host: host({ document }),
  writeStdout: () => {},
});
run();

const labelText = () => document.getElementById("label").textContent;
const out = [`run: ${labelText()}`];
const btn = document.getElementById("btn");
if (btn) {
  btn.dispatchEvent(new dom.window.Event("click", { bubbles: true }));
  out.push(`click: ${labelText()}`);
}
process.stdout.write(out.join("\n") + "\n");
