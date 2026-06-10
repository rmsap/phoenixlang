# Third-party notices

Phoenix is MIT-licensed (see [LICENSE](LICENSE)), and the repository
policy is MIT-only: no source derived from non-MIT-licensed projects is
vendored or ported into the tree (see `docs/design-decisions.md`,
§Phase 2.4 K.6 "Precomputed tables" for an example of the policy in
action). Ports of MIT-licensed code are permitted; the MIT license
requires that the upstream copyright and permission notice accompany
"all copies or substantial portions of the Software", so each such port
is listed here with its upstream notice reproduced in full.

## musl libc

`synthesize_fmod` in
`crates/phoenix-cranelift/src/wasm/wasm_gc/float_helpers.rs` is a port
of musl's `fmod` (`src/math/fmod.c`,
<https://musl.libc.org/>) into wasm-encoder bytecode.

```text
Copyright © 2005-2020 Rich Felker, et al.

Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
"Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE
LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION
WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```
