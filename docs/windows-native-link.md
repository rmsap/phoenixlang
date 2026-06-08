# Windows native-link

Status: **implemented**. `phoenix build --target native` produces and runs
native `.exe` files on Windows (x86_64-pc-windows-msvc), linked via MSVC
`link.exe`, alongside Linux and macOS. VS Build Tools is the prerequisite, the
same toolchain Rust requires. This doc records the design and the residual
risks; the original scope is kept below for context.

## How it works

- **`link.rs` `run_linker`** is `#[cfg(target_os = "windows")]`-split: Unix
  drives `cc`; Windows locates MSVC `link.exe` via
  `cc::windows_registry::find_tool` (vswhere-based, the same discovery rustc
  uses), applies the toolchain's `LIB`/`PATH`/`INCLUDE` env, and links the
  object + `phoenix_runtime.lib` into a console `.exe`. No Developer Command
  Prompt required.
- **`cc` crate** is a `[target.'cfg(windows)'.dependencies]` entry, so it is
  only pulled in on Windows hosts.
- **Driver** (`build.rs`) appends `std::env::consts::EXE_SUFFIX` to the
  default-derived native output name, so `phoenix build hello.phx` yields a
  runnable `hello.exe` on Windows (explicit `-o` is still verbatim).
- **Errors:** `LinkError::MsvcToolchainNotFound` gives an actionable
  "install VS Build Tools" message when `link.exe` can't be located.
- **Tests/CI:** a `#[cfg(windows)]` unit test compiles a trivial object with
  `cl.exe`, links it, and runs it; the `windows-check` CI job (windows-latest)
  lints the Windows cfg and runs that test on every push; the release
  `smoke-test` matrix builds and runs `hello.phx` natively on windows-latest
  from the shipped artifact.

## Residual risks (watch on first real release)

- **System-lib resolution** relies on the `/DEFAULTLIB` directives Rust embeds
  in `phoenix_runtime.lib`; if some symbol isn't covered, the fix is adding a
  specific lib to the Windows arm — surfaced by the smoke-test link step.
- **CRT flavor / ABI:** `default_call_conv()` should match the runtime's
  `extern "C"` Win64 ABI; the smoke + `windows-check` end-to-end run is what
  proves it on a real host (linux cross-`check` only type-checks).

---

## Original scope (for context)

The remainder is the pre-implementation scoping analysis.

Goal: close the gap so a downloaded Phoenix can produce native `.exe` files on
Windows, the way `rustc`, `go build`, and `zig` all do.

## Why it matters

A general-purpose compiled-language CLI that can't emit native Windows
executables on Windows is the exception, not the rule (Rust treats
`x86_64-pc-windows-msvc` as Tier 1; Go and Zig ship native Windows support).
We already ship a `windows-latest` binary and bundle `phoenix_runtime.lib`
into its archive, so the only missing capability is the link step.

## What already works on Windows today (no change needed)

- **`wasm32-linear` and `wasm32-gc` builds.** These never touch `cc`/`link.rs`
  — they merge/emit WebAssembly bytes in-process. With `phoenix_runtime.wasm`
  now bundled (see `release.yml`) the Windows binary is already a working wasm
  compiler. The `smoke-test` matrix exercises this on `windows-latest`.
- **Codegen targets the host triple.** `context.rs` builds the ISA from
  `Triple::host()` and uses `isa.default_call_conv()`, so on a Windows host
  Cranelift emits **COFF** objects with the **Windows x64 calling convention**
  automatically. That matches the runtime's `extern "C"` symbols (Win64 ABI),
  and on x64 there is no leading-underscore name mangling to reconcile.
- **Runtime artifact + discovery.** `RUNTIME_LIB_NAME` already resolves to
  `phoenix_runtime.lib` on Windows (`link.rs:13`), the release `build` job
  produces it, and `find_runtime_lib` is path-portable (it found the
  `bin/../lib` install layout in tests on all OSes).

So the gap is **only** the linker invocation in `link.rs`.

## The gap

`link_executable` (`link.rs:174`) hard-codes a Unix `cc` line:

```
cc -o <exe> <obj> <runtime>.a -lpthread -ldl -lm     # SUPPORTED_PLATFORMS: linux/macos only
```

`cc` is not the Windows linker, and `-lpthread -ldl -lm` are POSIX libs that
don't exist on Windows. `platform_link_args()` (`link.rs:83`) returns
`UnsupportedPlatform` for `os == "windows"`.

## Work items

1. **`SUPPORTED_PLATFORMS` + `platform_link_args` (`link.rs:76`).** Add a
   `windows` arm. A Rust staticlib embeds `/DEFAULTLIB` directives for the
   system libs it needs (kernel32, advapi32, ntdll, bcrypt, ws2_32, userenv,
   ucrt, vcruntime, …), so `link.exe` auto-resolves most of them — we should
   **not** hand-maintain that list. The Windows arm likely needs few or no
   extra `-l`-style libs; verify empirically.

2. **Linker invocation (`link_executable`).** `link.exe` syntax differs from
   `cc`: `/OUT:<exe>`, object + `.lib` as positional args, `/SUBSYSTEM:CONSOLE`,
   and the CRT supplies `mainCRTStartup` which calls our emitted `main`
   (confirm the object exports `main`, matching the Unix C-runtime contract).
   Branch on target OS rather than threading flags through the `cc` shape.

3. **Locating the MSVC toolchain.** Don't require the user to launch a
   Developer Command Prompt. Use the `cc` crate's
   `cc::windows_registry::find_tool("x86_64-pc-windows-msvc", "link.exe")`,
   which returns the linker path **and** the `LIB`/`PATH` environment it needs
   (vswhere under the hood) — the same mechanism `rustc` relies on. Document
   that VS Build Tools (or `clang`/`lld-link`) is a prerequisite, exactly as
   Rust documents it. Consider `lld-link` as a fallback if MSVC isn't found.

4. **Precheck + error variants.** Extend `precheck_link_environment`
   (`link.rs:238`, the test helper) and the production path to detect a missing
   `link.exe` with an actionable message (mirror `PHOENIX_REQUIRE_CC`). Add a
   distinct `LinkError` for "MSVC toolchain not found" vs the generic spawn
   failure.

5. **Tests.** The end-to-end `link_executable_succeeds_on_trivial_object` test
   is `#[cfg(unix)]` (`link.rs:321`). Add a `#[cfg(windows)]` sibling. The
   `RUNTIME_LIB_NAME` platform test already covers the `.lib` name.

6. **CI.** Flip the `windows-latest` row in `release.yml`'s `smoke-test` matrix
   to `native: true`; `windows-latest` ships MSVC, so it can both link and run
   the produced `.exe`. That turns this doc's success criterion into an
   enforced release gate.

7. **(Optional) Windows installer.** `install.sh` is POSIX-only; Windows users
   extract the release zip manually. Discovery already works if they place
   `phoenix.exe` in `bin/` and the libs in `../lib/`, or set
   `$PHOENIX_RUNTIME_LIB`. A `install.ps1` is a nice-to-have, out of scope here.

## Risks / unknowns

- **System-lib list.** If `/DEFAULTLIB` auto-resolution is incomplete for some
  runtime symbol, we may need to add specific libs — discoverable from the
  first failing link, low risk.
- **CRT flavor.** Static (`libcmt`) vs dynamic (`msvcrt`/ucrt) CRT must match
  how `phoenix_runtime.lib` was built by cargo (default is the dynamic ucrt).
  Mismatch shows up as duplicate/undefined CRT symbols at link time.
- **First-run ABI bugs.** `default_call_conv()` *should* be correct, but the
  first real Windows link is where any struct-return / stack-alignment / vararg
  ABI mismatch between Cranelift output and the Rust runtime would surface.

## Effort

Medium — roughly 1–3 focused days. The codegen and packaging are already in
place; the work is the `link.rs` Windows backend (items 1–4), tests (5), and
flipping the CI gate (6). Most of the risk is concentrated in toolchain
discovery and the first end-to-end link/run on a real Windows host.
