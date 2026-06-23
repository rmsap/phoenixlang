// Host stub for `interop_noop_extern`: a single do-nothing binding. `nop` is the
// cheapest possible host function — it returns immediately, so the measured cost
// is the boundary crossing itself (the glue thunk + the WASM→JS import call),
// not any work the host does.
export function host() {
  return {
    nop: () => {},
  };
}
