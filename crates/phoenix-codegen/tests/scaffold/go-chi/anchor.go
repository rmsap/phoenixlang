// Package gochi anchors the chi dependency with a blank import so it stays a
// direct, used requirement — a stray `go mod tidy` here won't prune the chi
// `require` (and its go.sum entries) that the generated `api` package, which the
// compile-and-lint harness drops in, needs. It is not built into anything.
package gochi

import _ "github.com/go-chi/chi/v5"
