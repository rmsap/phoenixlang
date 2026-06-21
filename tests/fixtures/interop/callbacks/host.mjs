// Host stub for the `callbacks` interop fixture: Phoenix closures handed to the
// host as callbacks. The host stubs are deterministic and synchronous (a *drained*
// `setTimeout` invokes its callback immediately), so output is byte-stable — the
// callbacks-only async model (design-decisions §Phase 2.5 decision H). Covers a
// no-arg `() -> Void`, an `(Int) -> Void`, and a value-returning `(Int) -> Int`.
export function host() {
  return {
    setTimeout: (cb, _ms) => { cb(); },
    eachUpTo: (n, cb) => { for (let i = 0; i < n; i++) cb(i); },
    sumMap: (n, cb) => {
      let acc = 0;
      for (let i = 0; i < n; i++) acc += cb(i);
      return acc;
    },
  };
}
