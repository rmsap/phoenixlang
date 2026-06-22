// DOM host for the `set_text` fixture. A DOM element crosses to Phoenix as an
// opaque `JsValue` handle (`getElementById`); `setText` mutates its text content
// — a host call that produces a real DOM effect. The `document` comes from `ctx`
// (the jsdom document under Node, the page's real `document` in a browser), so
// the same host runs unchanged in both tiers.
export function host({ document }) {
  return {
    getElementById: (id) => document.getElementById(id),
    setText: (el, text) => { el.textContent = text; },
  };
}
