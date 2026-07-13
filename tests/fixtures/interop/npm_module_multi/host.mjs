// Host stub for the `npm_module_multi` interop fixture: two
// Phoenix modules each declare the same `extern js "left-pad"` binding — the
// expected BYO pattern, where every module declares the npm exports it uses.
// The compiled output carries ONE `left-pad` import namespace with ONE
// `leftPad` thunk (the `(module, name)` pair dedupes), and both modules' call
// sites route through it — so one binding here serves both.
export function host() {
  return {
    "left-pad": {
      leftPad: (s, width) => s.padStart(width),
    },
  };
}
