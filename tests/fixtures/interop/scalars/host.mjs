// Host stub for the `scalars` interop fixture: Int / Bool / Float round-trips.
// Exports `host(ctx)` returning the `extern js` bindings. `ctx.emit(text)` (unused
// here) lets a host append directly to the captured output; this fixture's values
// flow back into the program and are printed by Phoenix `print`.
export function host() {
  return {
    addOne: (n) => n + 1,
    negate: (b) => !b,
    halve: (x) => x / 2,
    floorToInt: (x) => Math.floor(x),
  };
}
