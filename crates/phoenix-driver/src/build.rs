//! Native compilation via Cranelift.
//!
//! Implements the `phoenix build` subcommand: parses the source, type-checks,
//! lowers to IR, translates to Cranelift, emits an object file, and links
//! with the system linker and Phoenix runtime library.

use std::fs;
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
    let (program, check_result) = super::parse_and_check(path);
    let ir_module = phoenix_ir::lower(&program, &check_result.module);

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
    let out_path = match output {
        Some(p) => p.to_string(),
        None => {
            // Strip .phx extension from input path.
            let stem = std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("a.out");
            stem.to_string()
        }
    };

    link_object(&obj_bytes, &out_path);
}

/// Write an object file to a unique temp path, link it with the runtime,
/// and produce an executable.
fn link_object(obj_bytes: &[u8], out_path: &str) {
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
        std::path::Path::new(out_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("out")
    ));
    fs::write(&obj_path, obj_bytes).unwrap_or_else(|err| {
        eprintln!("error: could not write object file: {}", err);
        process::exit(1);
    });

    // Find the runtime library.
    let runtime_dir = match phoenix_cranelift::find_runtime_lib() {
        Some(dir) => dir,
        None => {
            eprintln!(
                "error: could not find {}\n\
                 Set $PHOENIX_RUNTIME_LIB to the directory containing it,\n\
                 or reinstall Phoenix with the install script.",
                phoenix_cranelift::RUNTIME_LIB_NAME,
            );
            process::exit(1);
        }
    };

    // Build platform-appropriate linker arguments.
    let mut cmd = std::process::Command::new("cc");
    cmd.arg("-o")
        .arg(out_path)
        .arg(obj_path.to_str().unwrap_or(""))
        .arg(format!("-L{runtime_dir}"))
        .arg("-lphoenix_runtime");

    // Platform-specific system libraries.
    if cfg!(target_os = "linux") {
        cmd.arg("-lpthread").arg("-ldl").arg("-lm");
    } else if cfg!(target_os = "macos") {
        cmd.arg("-lpthread").arg("-lm");
    }

    let status = cmd.status();

    match status {
        Ok(s) if s.success() => {
            // Clean up temp directory on success.
            let _ = fs::remove_dir_all(&tmp_dir);
            eprintln!("Compiled to {out_path}");
        }
        Ok(s) => {
            eprintln!(
                "error: linker exited with {} (object file kept at {})",
                s,
                obj_path.display()
            );
            process::exit(1);
        }
        Err(err) => {
            eprintln!(
                "error: could not run linker 'cc': {}\n\
                 Make sure a C compiler is installed (e.g. gcc or clang).\n\
                 Object file kept at {}.",
                err,
                obj_path.display()
            );
            process::exit(1);
        }
    }
}
