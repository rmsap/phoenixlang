// DOM host for the `click_handler` fixture. Adds `onClick`, which registers the
// Phoenix closure as a real DOM `click` listener — so a closure handed across the
// boundary and *retained* by the host (decision G) fires when the host later
// dispatches a click, mutating the DOM from inside the wasm callback. Same host
// for the jsdom and browser tiers (the `document` comes from `ctx`).
export function host({ document }) {
  return {
    getElementById: (id) => document.getElementById(id),
    setText: (el, text) => { el.textContent = text; },
    onClick: (el, handler) => { el.addEventListener("click", () => handler()); },
  };
}
