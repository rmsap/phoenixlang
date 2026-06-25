//! Compilation via Cranelift (native) / `wasm-encoder` (WASM).
//!
//! Implements the `phoenix build` subcommand: parses the source, type-checks,
//! lowers to IR, translates to the per-target backend, and emits an object
//! (native) or WebAssembly (wasm32-*) artifact, linking with the system
//! linker plus the Phoenix runtime library for native targets. The WASM
//! targets emit their artifact directly with no linker step. As of Phase
//! 2.4 PR 2, `wasm32-linear` is wired through to `phoenix-cranelift`'s
//! `wasm-encoder`-based pipeline; `wasm32-gc` still errors at `compile`
//! until its codegen lands in Phase 2.4 PR 5+.

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use phoenix_cranelift::Target;

/// Monotonic counter for unique temp file names across threads.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Compile a Phoenix program to an executable artifact.
///
/// Runs the full pipeline: parse → type-check → IR lower → verify →
/// Cranelift compile → link (native only).  On success, writes the
/// artifact to `output` (or the input filename without `.phx`).
///
/// `target_str` is the user-supplied `--target` value or `None` for
/// the default (native). Unknown values produce a single
/// "unknown target" diagnostic listing every accepted spelling.
pub fn cmd_build(path: &str, output: Option<&str>, target_str: Option<&str>, locked: bool) {
    let target = match target_str {
        Some(s) => match Target::from_cli(s) {
            Some(t) => t,
            None => {
                eprintln!(
                    "error: unknown --target `{s}`; expected one of: {}",
                    Target::all_cli_names().join(", "),
                );
                process::exit(1);
            }
        },
        None => Target::default(),
    };

    let (modules, check_result, sm) = super::parse_resolve_check(path, locked);
    if super::report_unlowerable_namespace_calls(&check_result, &sm) {
        process::exit(1);
    }
    let ir_module = phoenix_ir::lower_modules(&modules, &check_result.module);

    let errors = phoenix_ir::verify::verify(&ir_module);
    if !errors.is_empty() {
        for err in &errors {
            eprintln!("IR verification error in {}: {}", err.function, err.message);
        }
        process::exit(1);
    }

    // Compile IR to target-appropriate bytes.
    let artifact_bytes = match phoenix_cranelift::compile(&ir_module, target) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("error: {}", err);
            process::exit(1);
        }
    };

    // Determine output path. For WASM targets without an explicit
    // --output, append `.wasm` so the artifact has the extension that
    // every WASM consumer (wasmtime, browsers, Node loaders) expects;
    // for native targets append the platform executable suffix
    // (`.exe` on Windows, empty elsewhere) so the default-named output
    // is directly runnable. Explicit --output is taken verbatim —
    // caller's choice wins (including on Windows).
    let out_path: PathBuf = match output {
        Some(p) => PathBuf::from(p),
        None => {
            // Strip .phx extension from input path.
            let stem = Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("a.out");
            if target.is_wasm() {
                PathBuf::from(format!("{stem}.wasm"))
            } else {
                PathBuf::from(format!("{stem}{}", std::env::consts::EXE_SUFFIX))
            }
        }
    };

    // Native: write the object to a temp path and invoke the system linker.
    // WASM: write the module directly. Reachable for `Wasm32Linear` as of
    // Phase 2.4 PR 2; `Wasm32Gc` still errors at `compile` and never gets
    // here.
    if target.is_wasm() {
        // For a `wasm32-linear` or `wasm32-gc` module that uses `extern js`,
        // emit a paired `.js` glue sidecar next to the `.wasm` so the module can
        // be instantiated under Node / the browser (the glue provides WASI + the
        // host-import thunks). Programs without externs get no sidecar — the
        // bare `.wasm` runs under wasmtime as before. The two targets share the
        // glue core and differ only in the value-ABI marshalling (decision C).
        //
        // Generate the glue *before* writing the `.wasm`: a glue-generation
        // failure then aborts the build without leaving a stale `.wasm` behind
        // (an artifact whose paired sidecar never got written).
        // `is_wasm()` admits only these two targets, so the match is total here;
        // each binding dispatches to its backend's glue generator (decision C).
        let js_glue_result = match target {
            Target::Wasm32Linear => phoenix_cranelift::wasm_linear_js_glue(&ir_module),
            Target::Wasm32Gc => phoenix_cranelift::wasm_gc_js_glue(&ir_module),
            Target::Native => unreachable!("guarded by `target.is_wasm()`"),
        };
        let js_glue = match js_glue_result {
            Ok(glue) => glue,
            Err(err) => {
                eprintln!("error: generating JS glue: {err}");
                process::exit(1);
            }
        };

        if let Err(err) = fs::write(&out_path, &artifact_bytes) {
            eprintln!(
                "error: could not write WASM module to {}: {err}",
                out_path.display()
            );
            process::exit(1);
        }

        // The paired glue sidecar's path is a pure function of the artifact
        // path: swap a trailing `.wasm` for `.js` (the common `app.wasm` →
        // `app.js` case), but if the artifact has any other extension — e.g. an
        // explicit `-o app.foo` — append `.js` instead of clobbering it, so the
        // sidecar always sits beside the artifact with its stem intact. Computed
        // unconditionally because the no-extern branch also needs it to clean up
        // a stale sidecar left by a previous extern-using build. The extension
        // match is case-insensitive so a `-o app.WASM` still swaps to `app.js`
        // rather than appending (`app.WASM.js`).
        let js_path = if out_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("wasm"))
        {
            out_path.with_extension("js")
        } else {
            let mut p = out_path.clone().into_os_string();
            p.push(".js");
            PathBuf::from(p)
        };

        // Write the sidecar *after* the `.wasm`, but treat the pair atomically:
        // if the sidecar write fails, remove the `.wasm` we just wrote so we
        // never leave an extern-using `.wasm` whose paired glue is missing.
        // Hold the success message until both land for the same reason.
        if let Some(glue) = js_glue {
            // The paired sidecar lives at a fixed path beside the `.wasm`, so an
            // extern-using build must write it there — unlike the no-extern
            // branch below, it can't honour the generated-code marker (there's
            // nowhere else to put the glue). Warn before clobbering a file that
            // lacks the marker, so a user who kept a hand-written `.js` and then
            // added an `extern` block doesn't lose it silently.
            if let Ok(existing) = fs::read_to_string(&js_path)
                && !existing.starts_with(phoenix_cranelift::GENERATED_GLUE_MARKER)
            {
                eprintln!(
                    "warning: overwriting {} with generated JS glue \
                     (it lacks the Phoenix generated-code marker — was it hand-written?)",
                    js_path.display()
                );
            }
            if let Err(err) = fs::write(&js_path, glue) {
                eprintln!(
                    "error: could not write JS glue to {}: {err}",
                    js_path.display()
                );
                // Treat the pair atomically: drop the `.wasm` we just wrote so
                // we never leave an extern-using `.wasm` whose paired glue is
                // missing. If even the cleanup fails, say so — silently
                // swallowing it would leave that exact broken state behind
                // while the build still reports only the glue error.
                if let Err(rm_err) = fs::remove_file(&out_path) {
                    eprintln!(
                        "warning: could not remove the orphaned WASM module {}: {rm_err}",
                        out_path.display()
                    );
                }
                process::exit(1);
            }
            eprintln!("Compiled to {}", out_path.display());
            eprintln!("Wrote JS glue to {}", js_path.display());
        } else {
            // No glue this build. `js_glue` is only ever `Some` for
            // `wasm32-linear` (it's `None` for any other wasm target by
            // construction above), so in practice this branch runs for a
            // linear build with no externs, or — once it lands — `wasm32-gc`,
            // which today errors at `compile` and never reaches here. Either
            // way the stale-sidecar cleanup is sound: it only removes a
            // marker-bearing file.
            //
            // If a *previous* build of this artifact emitted a glue sidecar, it
            // is now stale — leaving it would let a consumer import glue for a
            // module that no longer declares those imports. Remove it, but only
            // if it carries the generated-code marker, so a hand-written `.js`
            // the user keeps beside the `.wasm` is never clobbered.
            if let Ok(existing) = fs::read_to_string(&js_path)
                && existing.starts_with(phoenix_cranelift::GENERATED_GLUE_MARKER)
            {
                match fs::remove_file(&js_path) {
                    Ok(()) => eprintln!("Removed stale JS glue {}", js_path.display()),
                    Err(err) => eprintln!(
                        "warning: could not remove stale JS glue {}: {err}",
                        js_path.display()
                    ),
                }
            }
            eprintln!("Compiled to {}", out_path.display());
        }
    } else {
        link_object(&artifact_bytes, &out_path);
    }
}

/// Write an object file to a unique temp path, link it with the runtime,
/// and produce an executable.
fn link_object(obj_bytes: &[u8], out_path: &Path) {
    // Use PID + counter for unique temp paths to avoid collisions when
    // multiple `phoenix build` invocations run concurrently.
    let pid = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_dir = std::env::temp_dir().join(format!("phoenix_build_{pid}_{n}"));
    fs::create_dir_all(&tmp_dir).unwrap_or_else(|err| {
        eprintln!("error: could not create temp directory: {}", err);
        process::exit(1);
    });
    let obj_path = tmp_dir.join(format!(
        "{}.o",
        out_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("out")
    ));
    fs::write(&obj_path, obj_bytes).unwrap_or_else(|err| {
        eprintln!("error: could not write object file: {}", err);
        process::exit(1);
    });

    match phoenix_cranelift::link_executable(&obj_path, out_path) {
        Ok(()) => {
            let _ = fs::remove_dir_all(&tmp_dir);
            eprintln!("Compiled to {}", out_path.display());
        }
        Err(err) => {
            // Only the linker-was-actually-invoked variants benefit from
            // a "kept the obj file" hint — for the others the user just
            // needs to fix their environment.
            let keep_hint = matches!(
                err,
                phoenix_cranelift::LinkError::LinkerFailed(_)
                    | phoenix_cranelift::LinkError::SpawnLinker(_)
            );
            if keep_hint {
                eprintln!("error: {err} (object file kept at {})", obj_path.display());
            } else {
                eprintln!("error: {err}");
            }
            process::exit(1);
        }
    }
}
