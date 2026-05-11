//! Native compilation via Cranelift.
//!
//! Implements the `phoenix build` subcommand: parses the source, type-checks,
//! lowers to IR, translates to Cranelift, emits an object file, and links
//! with the system linker and Phoenix runtime library.

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counter for unique temp file names across threads.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Compile a Phoenix program to a native executable.
///
/// Runs the full pipeline: parse → type-check → IR lower → verify →
/// Cranelift compile → link.  On success, writes the executable to
/// `output` (or the input filename without `.phx`).
pub(crate) fn cmd_build(path: &str, output: Option<&str>) {
    let (modules, check_result, _sm) = super::parse_resolve_check(path);
    let ir_module = phoenix_ir::lower_modules(&modules, &check_result.module);

    let errors = phoenix_ir::verify::verify(&ir_module);
    if !errors.is_empty() {
        for err in &errors {
            eprintln!("IR verification error in {}: {}", err.function, err.message);
        }
        process::exit(1);
    }

    // Compile IR to object file bytes.
    let obj_bytes = match phoenix_cranelift::compile(&ir_module) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("error: {}", err);
            process::exit(1);
        }
    };

    // Determine output path.
    let out_path: PathBuf = match output {
        Some(p) => PathBuf::from(p),
        None => {
            // Strip .phx extension from input path.
            let stem = Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("a.out");
            PathBuf::from(stem)
        }
    };

    link_object(&obj_bytes, &out_path);
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
