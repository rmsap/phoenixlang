//! Shared test helpers for Cranelift compilation integration tests.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use phoenix_common::span::SourceId;
use phoenix_lexer::lexer::tokenize;
use phoenix_parser::parser;
use phoenix_sema::checker;

/// Monotonic counter to generate unique temp file names across threads.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Compile Phoenix source to an object file, link it, run the binary, and
/// return the captured stdout lines.
pub fn compile_and_run(source: &str) -> Vec<String> {
    let obj_bytes = compile_to_obj(source);
    let (obj_path, exe_path) = link_binary(&obj_bytes, "test");

    // Run.
    let output = Command::new(exe_path.to_str().unwrap())
        .output()
        .expect("could not run compiled binary");
    assert!(
        output.status.success(),
        "binary exited with {}: stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // Clean up.
    let _ = std::fs::remove_file(&obj_path);
    let _ = std::fs::remove_file(&exe_path);

    let stdout = String::from_utf8(output.stdout).unwrap();
    stdout.lines().map(|l| l.to_string()).collect()
}

/// Compile Phoenix source to object bytes (panics on failure).
pub fn compile_to_obj(source: &str) -> Vec<u8> {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "type errors: {:?}",
        result.diagnostics
    );
    let module = phoenix_ir::lower(&program, &result);
    let errors = phoenix_ir::verify::verify(&module);
    assert!(errors.is_empty(), "IR verification errors: {:?}", errors);
    phoenix_cranelift::compile(&module).expect("compilation failed")
}

/// Run source through the IR interpreter and capture print() output.
pub fn ir_run(source: &str) -> Vec<String> {
    let tokens = tokenize(source, SourceId(0));
    let (program, parse_errors) = parser::parse(&tokens);
    assert!(parse_errors.is_empty(), "parse errors: {:?}", parse_errors);
    let result = checker::check(&program);
    assert!(
        result.diagnostics.is_empty(),
        "type errors: {:?}",
        result.diagnostics
    );
    let module = phoenix_ir::lower(&program, &result);
    let errors = phoenix_ir::verify::verify(&module);
    assert!(errors.is_empty(), "IR verification errors: {:?}", errors);
    phoenix_ir_interp::run_and_capture(&module).expect("IR runtime error")
}

/// Assert that compiled output matches the IR interpreter output.
pub fn roundtrip(source: &str) {
    let ir_out = ir_run(source);
    let compiled_out = compile_and_run(source);
    assert_eq!(
        ir_out, compiled_out,
        "output mismatch\n  IR:       {:?}\n  Compiled: {:?}",
        ir_out, compiled_out
    );
}

/// Find the directory containing the Phoenix runtime static library.
pub fn runtime_dir() -> String {
    phoenix_cranelift::find_runtime_lib()
        .expect("could not find runtime lib — build it first with `cargo build -p phoenix-runtime`")
}

/// Compile object bytes to a linked executable and return (obj_path, exe_path).
///
/// Shared by [`compile_and_run`] and [`expect_panic`] to avoid duplicating
/// the linking logic (temp file creation, linker flags, platform libs).
fn link_binary(obj_bytes: &[u8], prefix: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join("phoenix_cranelift_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let id = std::process::id();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let obj_path = dir.join(format!("{prefix}_{id}_{n}.o"));
    let exe_path = dir.join(format!("{prefix}_{id}_{n}"));
    std::fs::write(&obj_path, obj_bytes).unwrap();

    let rt_dir = runtime_dir();
    let mut cmd = Command::new("cc");
    cmd.arg("-o")
        .arg(exe_path.to_str().unwrap())
        .arg(obj_path.to_str().unwrap())
        .arg(format!("-L{rt_dir}"))
        .arg("-lphoenix_runtime");

    // Platform-specific system libraries.
    if cfg!(target_os = "linux") {
        cmd.arg("-lpthread").arg("-ldl").arg("-lm");
    } else if cfg!(target_os = "macos") {
        cmd.arg("-lpthread").arg("-lm");
    }

    let status = cmd.status().expect("could not run linker 'cc'");
    assert!(status.success(), "linking failed: {status}");
    (obj_path, exe_path)
}

/// Compile a Phoenix program, link it, run it, and assert it panics with
/// the expected message on stderr.
pub fn expect_panic(source: &str, expected_stderr: &str) {
    let obj_bytes = compile_to_obj(source);
    let (obj_path, exe_path) = link_binary(&obj_bytes, "test_panic");

    let output = Command::new(exe_path.to_str().unwrap())
        .output()
        .expect("could not run compiled binary");
    assert!(
        !output.status.success(),
        "expected non-zero exit, but binary succeeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_stderr),
        "expected stderr to contain {expected_stderr:?}, got: {stderr}"
    );
    let _ = std::fs::remove_file(&obj_path);
    let _ = std::fs::remove_file(&exe_path);
}
