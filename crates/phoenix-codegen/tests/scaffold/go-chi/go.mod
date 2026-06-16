module gochi

go 1.23

// This `require` is the single source of truth for the pinned chi version: the
// round-trip test reads it back from here (see `chi_require_from_scaffold` in
// tests/roundtrip.rs) rather than hardcoding a copy. Bump with
// `go get github.com/go-chi/chi/v5@<version>` here (which also refreshes go.sum).
// chi is NOT vendored — `go build`/`go test` resolve it from the module cache/proxy.
require github.com/go-chi/chi/v5 v5.3.0
