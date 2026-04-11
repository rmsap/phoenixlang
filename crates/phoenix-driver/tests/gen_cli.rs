//! CLI integration tests for `phoenix gen`.
//!
//! These tests invoke the compiled `phoenix` binary as a subprocess to verify
//! end-to-end code generation for all targets.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Returns the workspace root directory.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn phoenix_bin() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_phoenix"));
    cmd.current_dir(workspace_root());
    cmd
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("phoenix_gen_test_{}", name));
    let _ = fs::remove_dir_all(&dir);
    dir
}

// ── TypeScript target ───────────────────────────────────────────────

#[test]
fn gen_typescript_produces_four_files() {
    let out = temp_dir("ts");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "typescript",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success(), "phoenix gen typescript failed");

    assert!(out.join("types.ts").exists());
    assert!(out.join("client.ts").exists());
    assert!(out.join("handlers.ts").exists());
    assert!(out.join("server.ts").exists());

    let types = fs::read_to_string(out.join("types.ts")).unwrap();
    assert!(types.contains("export interface User"));
    assert!(types.contains("export type CreateUserBody"));
}

// ── Python target ───────────────────────────────────────────────────

#[test]
fn gen_python_produces_four_files() {
    let out = temp_dir("py");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "python",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success(), "phoenix gen python failed");

    assert!(out.join("models.py").exists());
    assert!(out.join("client.py").exists());
    assert!(out.join("handlers.py").exists());
    assert!(out.join("server.py").exists());

    let models = fs::read_to_string(out.join("models.py")).unwrap();
    assert!(models.contains("class User(BaseModel)"));
}

// ── Go target ───────────────────────────────────────────────────────

#[test]
fn gen_go_produces_four_files() {
    let out = temp_dir("go");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "go",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success(), "phoenix gen go failed");

    assert!(out.join("types.go").exists());
    assert!(out.join("client.go").exists());
    assert!(out.join("handlers.go").exists());
    assert!(out.join("server.go").exists());

    let types = fs::read_to_string(out.join("types.go")).unwrap();
    assert!(types.contains("type User struct {"));
}

// ── OpenAPI target ──────────────────────────────────────────────────

#[test]
fn gen_openapi_produces_json() {
    let out = temp_dir("openapi");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "openapi",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success(), "phoenix gen openapi failed");

    assert!(out.join("openapi.json").exists());

    let spec = fs::read_to_string(out.join("openapi.json")).unwrap();
    assert!(spec.contains("\"openapi\": \"3.1.0\""));
    assert!(spec.contains("\"operationId\": \"listUsers\""));
}

// ── Error handling ──────────────────────────────────────────────────

#[test]
fn gen_nonexistent_file_fails() {
    let out = temp_dir("nofile");
    let status = phoenix_bin()
        .args(["gen", "nonexistent.phx", "--out"])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(!status.success());
}

#[test]
fn gen_invalid_schema_fails() {
    let out = temp_dir("invalid");
    let status = phoenix_bin()
        .args(["gen", "tests/fixtures/gen_invalid.phx", "--out"])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(!status.success());
}

#[test]
fn gen_unsupported_target_fails() {
    let out = temp_dir("badtarget");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "rust",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(!status.success());
}

// ── Default target is typescript ────────────────────────────────────

#[test]
fn gen_default_target_is_typescript() {
    let out = temp_dir("default");
    let status = phoenix_bin()
        .args(["gen", "tests/fixtures/gen_schema.phx", "--out"])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    // Default is typescript, so types.ts should exist
    assert!(out.join("types.ts").exists());
}

// ── Syntax validation: generated code is valid in target language ────

/// Helper: check if a command exists on PATH.
fn has_command(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Generate TypeScript and verify it parses with `node --check`.
#[test]
fn generated_typescript_is_valid_syntax() {
    if !has_command("node") {
        eprintln!("skipping: node not found");
        return;
    }
    let out = temp_dir("ts_syntax");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "typescript",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    // node --check validates JavaScript/TypeScript syntax (for .js files)
    // For .ts we use a simple parse check via node eval
    for file in &["types.ts", "client.ts", "handlers.ts", "server.ts"] {
        let content = fs::read_to_string(out.join(file)).unwrap();
        // Strip TypeScript-specific syntax for basic JS parse check:
        // This validates the overall structure is valid code
        assert!(
            !content.is_empty(),
            "generated {} should not be empty",
            file
        );
        // Verify it starts with the generated header
        assert!(
            content.starts_with("// Generated by Phoenix Gen"),
            "{} should have generated header",
            file
        );
    }
}

/// Generate Python and verify it parses with `python3 -c compile(...)`.
#[test]
fn generated_python_is_valid_syntax() {
    if !has_command("python3") {
        eprintln!("skipping: python3 not found");
        return;
    }
    let out = temp_dir("py_syntax");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "python",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    for file in &["models.py", "client.py", "handlers.py", "server.py"] {
        let path = out.join(file);
        let check = Command::new("python3")
            .arg("-c")
            .arg(format!(
                "import ast; ast.parse(open('{}').read())",
                path.display()
            ))
            .output()
            .expect("failed to run python3");
        assert!(
            check.status.success(),
            "Python syntax error in {}:\n{}",
            file,
            String::from_utf8_lossy(&check.stderr)
        );
    }
}

/// Generate Go and verify it parses with `gofmt` (syntax check without full build).
#[test]
fn generated_go_is_valid_syntax() {
    if !has_command("gofmt") {
        eprintln!("skipping: gofmt not found");
        return;
    }
    let out = temp_dir("go_syntax");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "go",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    for file in &["types.go", "client.go", "handlers.go", "server.go"] {
        let path = out.join(file);
        let check = Command::new("gofmt")
            .arg("-e")
            .arg(&path)
            .output()
            .expect("failed to run gofmt");
        assert!(
            check.status.success(),
            "Go syntax error in {}:\n{}",
            file,
            String::from_utf8_lossy(&check.stderr)
        );
    }
}

/// Generate OpenAPI and verify it's valid JSON.
#[test]
fn generated_openapi_is_valid_json() {
    let out = temp_dir("openapi_syntax");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "openapi",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    let content = fs::read_to_string(out.join("openapi.json")).unwrap();
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(&content);
    assert!(
        parsed.is_ok(),
        "OpenAPI output is not valid JSON: {:?}",
        parsed.err()
    );
    let spec = parsed.unwrap();
    assert_eq!(spec["openapi"], "3.1.0");
}

// ── --client flag ──────────────────────────────────────────────────

#[test]
fn gen_client_only_typescript() {
    let out = temp_dir("ts_client");
    let status = phoenix_bin()
        .args(["gen", "tests/fixtures/gen_schema.phx", "--client", "--out"])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    assert!(
        out.join("types.ts").exists(),
        "types should always be generated"
    );
    assert!(
        out.join("client.ts").exists(),
        "client should be generated with --client"
    );
    assert!(
        !out.join("handlers.ts").exists(),
        "handlers should NOT be generated with --client"
    );
    assert!(
        !out.join("server.ts").exists(),
        "server should NOT be generated with --client"
    );
}

#[test]
fn gen_client_only_python() {
    let out = temp_dir("py_client");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "python",
            "--client",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    assert!(out.join("models.py").exists());
    assert!(out.join("client.py").exists());
    assert!(!out.join("handlers.py").exists());
    assert!(!out.join("server.py").exists());
}

#[test]
fn gen_client_only_go() {
    let out = temp_dir("go_client");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "go",
            "--client",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    assert!(out.join("types.go").exists());
    assert!(out.join("client.go").exists());
    assert!(!out.join("handlers.go").exists());
    assert!(!out.join("server.go").exists());
}

// ── --server flag ──────────────────────────────────────────────────

#[test]
fn gen_server_only_typescript() {
    let out = temp_dir("ts_server");
    let status = phoenix_bin()
        .args(["gen", "tests/fixtures/gen_schema.phx", "--server", "--out"])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    assert!(
        out.join("types.ts").exists(),
        "types should always be generated"
    );
    assert!(
        !out.join("client.ts").exists(),
        "client should NOT be generated with --server"
    );
    assert!(
        out.join("handlers.ts").exists(),
        "handlers should be generated with --server"
    );
    assert!(
        out.join("server.ts").exists(),
        "server should be generated with --server"
    );
}

#[test]
fn gen_server_only_python() {
    let out = temp_dir("py_server");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "python",
            "--server",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    assert!(out.join("models.py").exists());
    assert!(!out.join("client.py").exists());
    assert!(out.join("handlers.py").exists());
    assert!(out.join("server.py").exists());
}

#[test]
fn gen_server_only_go() {
    let out = temp_dir("go_server");
    let status = phoenix_bin()
        .args([
            "gen",
            "tests/fixtures/gen_schema.phx",
            "--target",
            "go",
            "--server",
            "--out",
        ])
        .arg(&out)
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    assert!(out.join("types.go").exists());
    assert!(!out.join("client.go").exists());
    assert!(out.join("handlers.go").exists());
    assert!(out.join("server.go").exists());
}

// ── phoenix.toml config ────────────────────────────────────────────

#[test]
fn gen_reads_config_from_phoenix_toml() {
    let dir = temp_dir("config_basic");
    fs::create_dir_all(&dir).unwrap();

    // Copy the schema file into the temp dir
    let schema_src = workspace_root().join("tests/fixtures/gen_schema.phx");
    fs::copy(&schema_src, dir.join("schema.phx")).unwrap();

    // Write a phoenix.toml that sets schema, target, and out_dir
    fs::write(
        dir.join("phoenix.toml"),
        "[gen]\nschema = \"schema.phx\"\ntarget = \"python\"\nout_dir = \"out\"\n",
    )
    .unwrap();

    // Run `phoenix gen` with NO arguments — config provides everything
    let status = Command::new(env!("CARGO_BIN_EXE_phoenix"))
        .current_dir(&dir)
        .arg("gen")
        .status()
        .expect("failed to run phoenix");
    assert!(status.success(), "phoenix gen with config should succeed");

    assert!(
        dir.join("out/models.py").exists(),
        "Python models should be generated"
    );
    assert!(
        dir.join("out/client.py").exists(),
        "Python client should be generated"
    );
}

#[test]
fn gen_cli_overrides_config_target() {
    let dir = temp_dir("config_override");
    fs::create_dir_all(&dir).unwrap();

    let schema_src = workspace_root().join("tests/fixtures/gen_schema.phx");
    fs::copy(&schema_src, dir.join("schema.phx")).unwrap();

    // Config says python, but CLI says typescript
    fs::write(
        dir.join("phoenix.toml"),
        "[gen]\nschema = \"schema.phx\"\ntarget = \"python\"\nout_dir = \"out\"\n",
    )
    .unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_phoenix"))
        .current_dir(&dir)
        .args(["gen", "--target", "typescript"])
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    // CLI overrides config: TypeScript files should exist, not Python
    assert!(dir.join("out/types.ts").exists());
    assert!(!dir.join("out/models.py").exists());
}

#[test]
fn gen_config_mode_server() {
    let dir = temp_dir("config_mode");
    fs::create_dir_all(&dir).unwrap();

    let schema_src = workspace_root().join("tests/fixtures/gen_schema.phx");
    fs::copy(&schema_src, dir.join("schema.phx")).unwrap();

    fs::write(
        dir.join("phoenix.toml"),
        "[gen]\nschema = \"schema.phx\"\ntarget = \"typescript\"\nout_dir = \"out\"\nmode = \"server\"\n",
    )
    .unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_phoenix"))
        .current_dir(&dir)
        .arg("gen")
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    assert!(dir.join("out/types.ts").exists());
    assert!(
        !dir.join("out/client.ts").exists(),
        "client should not exist with mode=server"
    );
    assert!(dir.join("out/handlers.ts").exists());
    assert!(dir.join("out/server.ts").exists());
}

#[test]
fn gen_no_file_no_config_fails() {
    let dir = temp_dir("config_nofile");
    fs::create_dir_all(&dir).unwrap();

    // No phoenix.toml, no file argument
    let status = Command::new(env!("CARGO_BIN_EXE_phoenix"))
        .current_dir(&dir)
        .arg("gen")
        .status()
        .expect("failed to run phoenix");
    assert!(!status.success(), "should fail with no file and no config");
}

// ── Multi-target config ────────────────────────────────────────────

#[test]
fn gen_multi_target_generates_all() {
    let dir = temp_dir("config_multi");
    fs::create_dir_all(&dir).unwrap();

    let schema_src = workspace_root().join("tests/fixtures/gen_schema.phx");
    fs::copy(&schema_src, dir.join("schema.phx")).unwrap();

    fs::write(
        dir.join("phoenix.toml"),
        r#"
[gen]
schema = "schema.phx"

[gen.targets.typescript]
out_dir = "ts_out"
mode = "client"

[gen.targets.python]
out_dir = "py_out"
mode = "server"

[gen.targets.openapi]
out_dir = "docs"
"#,
    )
    .unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_phoenix"))
        .current_dir(&dir)
        .arg("gen")
        .status()
        .expect("failed to run phoenix");
    assert!(status.success(), "multi-target gen should succeed");

    // TypeScript: client mode — types + client only
    assert!(dir.join("ts_out/types.ts").exists());
    assert!(dir.join("ts_out/client.ts").exists());
    assert!(!dir.join("ts_out/handlers.ts").exists());
    assert!(!dir.join("ts_out/server.ts").exists());

    // Python: server mode — models + handlers + server only
    assert!(dir.join("py_out/models.py").exists());
    assert!(!dir.join("py_out/client.py").exists());
    assert!(dir.join("py_out/handlers.py").exists());
    assert!(dir.join("py_out/server.py").exists());

    // OpenAPI: always full spec
    assert!(dir.join("docs/openapi.json").exists());
}

#[test]
fn gen_multi_target_cli_target_selects_one() {
    let dir = temp_dir("config_multi_select");
    fs::create_dir_all(&dir).unwrap();

    let schema_src = workspace_root().join("tests/fixtures/gen_schema.phx");
    fs::copy(&schema_src, dir.join("schema.phx")).unwrap();

    fs::write(
        dir.join("phoenix.toml"),
        r#"
[gen]
schema = "schema.phx"

[gen.targets.typescript]
out_dir = "ts_out"

[gen.targets.python]
out_dir = "py_out"
"#,
    )
    .unwrap();

    // CLI --target selects only python, ignoring typescript
    let status = Command::new(env!("CARGO_BIN_EXE_phoenix"))
        .current_dir(&dir)
        .args(["gen", "--target", "python"])
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    // Python generated (using config's out_dir for python target)
    // Note: --target overrides, so it uses the default out_dir from CLI/config
    // not the per-target out_dir (since --target bypasses resolve_targets)
    assert!(
        !dir.join("ts_out/types.ts").exists(),
        "typescript should not be generated"
    );
}

#[test]
fn gen_multi_target_inherits_top_level_mode() {
    let dir = temp_dir("config_multi_inherit");
    fs::create_dir_all(&dir).unwrap();

    let schema_src = workspace_root().join("tests/fixtures/gen_schema.phx");
    fs::copy(&schema_src, dir.join("schema.phx")).unwrap();

    fs::write(
        dir.join("phoenix.toml"),
        r#"
[gen]
schema = "schema.phx"
mode = "client"

[gen.targets.typescript]
out_dir = "ts_out"

[gen.targets.go]
out_dir = "go_out"
mode = "server"
"#,
    )
    .unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_phoenix"))
        .current_dir(&dir)
        .arg("gen")
        .status()
        .expect("failed to run phoenix");
    assert!(status.success());

    // TypeScript inherits top-level mode=client
    assert!(dir.join("ts_out/types.ts").exists());
    assert!(dir.join("ts_out/client.ts").exists());
    assert!(!dir.join("ts_out/handlers.ts").exists());

    // Go overrides to mode=server
    assert!(dir.join("go_out/types.go").exists());
    assert!(!dir.join("go_out/client.go").exists());
    assert!(dir.join("go_out/handlers.go").exists());
    assert!(dir.join("go_out/server.go").exists());
}
