//! Compilation via Cranelift.
//!
//! Implements the `phoenix build` subcommand: parses the source, type-checks,
//! lowers to IR, translates to Cranelift, emits an object (native) or
//! WebAssembly (wasm32-*) artifact, and links it with the system linker
//! plus the Phoenix runtime library for native targets. The WASM targets
//! emit their artifact directly with no linker step; their codegen lands
//! in Phase 2.4 — until then they error early at compile time.

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
pub(crate) fn cmd_build(path: &str, output: Option<&str>, target_str: Option<&str>) {
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

    let (modules, check_result, _sm) = super::parse_resolve_check(path);
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
    // every WASM consumer (wasmtime, browsers, Node loaders) expects.
    // Explicit --output is taken verbatim — caller's choice wins.
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
                PathBuf::from(stem)
            }
        }
    };

    // Native: write the object to a temp path and invoke the system linker.
    // WASM: write the module directly. (Unreachable today — wasm targets
    // error at `compile` until phase 2.4 completes — but the dispatch is in place so the
    // change is a localized addition rather than a control-flow edit.)
    if target.is_wasm() {
        if let Err(err) = fs::write(&out_path, &artifact_bytes) {
            eprintln!(
                "error: could not write WASM module to {}: {err}",
                out_path.display()
            );
            process::exit(1);
        }
        eprintln!("Compiled to {}", out_path.display());
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
