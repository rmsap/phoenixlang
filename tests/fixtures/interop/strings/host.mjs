// Host stub for the `strings` interop fixture: a `String` crosses *into* the host
// (read from linear memory) and a `String` crosses *back out* (the glue copies the
// returned JS string into a GC-managed Phoenix string via `phx_string_alloc`).
export function host() {
  return {
    shout: (s) => s.toUpperCase(),
    lengthOf: (s) => s.length,
  };
}
