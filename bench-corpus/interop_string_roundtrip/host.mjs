// Host stub for `interop_string_roundtrip`: `echo` returns its argument
// unchanged. The host does no work of its own — it just hands the string back —
// so the measured cost over `interop_noop_extern` is the glue's `String`
// marshalling (out: copy into the host string; in: copy into a fresh GC-managed
// Phoenix string), not anything the host computes.
export function host() {
  return {
    echo: (s) => s,
  };
}
