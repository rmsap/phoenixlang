// Host stub for the `host_effect` interop fixture: a host function with a
// side-effect (`alert`) rather than a return value. It writes through `ctx.emit`,
// which appends to the *same* captured buffer the harness collects Phoenix `print`
// into — so the assertion pins the interleaving of host output and program output
// (alert lands between `print(1)` and `print(2)`).
export function host({ emit }) {
  return {
    alert: (m) => { emit("ALERT: " + m + "\n"); },
  };
}
