// Host stub for the `interop_pure_call` boundary-cost baseline. The program
// declares one extern (`marker`) and calls it once — solely so the build emits
// the glue and runs through the same Node-driver harness as the other interop
// workloads (the timed loop itself never crosses the boundary). `marker` does
// nothing.
export function host() {
  return {
    marker: () => {},
  };
}
