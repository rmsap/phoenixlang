// Host stub for the `strings_unicode` interop fixture: a non-ASCII (multi-byte
// UTF-8) `String` round-trips through the host. `echo` returns it unchanged (in
// + out byte fidelity); `byteLen` reports its UTF-8 byte length — the one length
// measure identical across every backend, because UTF-8 bytes are exactly what
// crossed the wire. (JS `.length` would report UTF-16 code units, so we encode.)
export function host() {
  return {
    echo: (s) => s,
    byteLen: (s) => new TextEncoder().encode(s).length,
  };
}
