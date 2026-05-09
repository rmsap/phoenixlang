# phoenix-runtime

Runtime library for compiled [Phoenix](https://github.com/rmsap/phoenixlang) programs. Built as both a `staticlib` (`libphoenix_runtime.a`) and an `rlib`, it is linked into every native Phoenix executable produced by `phoenix build`.

The runtime provides the C-ABI symbols that compiled code calls via `extern` declarations: heap allocation through a tracing mark-and-sweep GC, a shadow stack for root tracking, and the built-in method implementations for `String`, `List`, and `Map`.

## How it gets used

In normal use you don't depend on this crate directly — the `phoenix build` driver locates the prebuilt static library and links it into your binary. Compiled Phoenix code emits calls like:

```text
phx_alloc, phx_string_alloc, phx_gc_alloc
phx_gc_push_frame, phx_gc_set_root, phx_gc_pop_frame
phx_str_concat, phx_list_alloc, phx_map_alloc, ...
```

The shadow stack contract — push a frame on entry, set roots before any allocation, pop on return — is the responsibility of the code generator (see `phoenix-cranelift`); the runtime just walks the roots during collection.

## Documentation

For the GC's design and rationale, see [`docs/design-decisions.md`](../../docs/design-decisions.md) (search for "GC implementation"). For the Rust-level API surface:

```
cargo doc -p phoenix-runtime --open
```

## License

MIT — see [LICENSE](../../LICENSE).
