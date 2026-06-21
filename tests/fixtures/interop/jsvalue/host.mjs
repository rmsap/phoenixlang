// Host stub for the `jsvalue` interop fixture: `JsValue` is an opaque handle into
// a glue-owned table mapping i32 handles to real JS objects. Phoenix never
// inspects the object — it only round-trips the handle back to the host. Identity
// is by handle: `sameNode(a, a)` compares the same object, `sameNode(a, b)` two
// distinct ones.
export function host() {
  const nodes = { x: { tag: "DIV" }, y: { tag: "SPAN" } };
  return {
    getEl: (id) => nodes[id],
    tagOf: (el) => el.tag,
    sameNode: (a, b) => a === b,
  };
}
