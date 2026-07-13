// Host stub for the `npm_module` interop fixture: an npm-package
// extern (`extern js "left-pad" { ... }`) binds on a nested namespace keyed by
// the package specifier, next to a flat ambient binding — proving the glue
// routes the two separately. This is the BYO shape: in a real embedding the
// nested object comes from the embedder's own `import` of the package; here an
// inline stand-in re-expresses left-pad's behavior.
export function host() {
  return {
    shout: (s) => s.toUpperCase(),
    "left-pad": {
      leftPad: (s, width) => s.padStart(width),
    },
  };
}
