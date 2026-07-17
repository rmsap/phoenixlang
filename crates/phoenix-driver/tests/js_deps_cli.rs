//! CLI tests for `[js-dependencies]` + `package.json` emission.
//!
//! A `phoenix build --target wasm32-linear` emits the `.wasm` + JS glue and,
//! from the project's `[js-dependencies]`, a `package.json` beside the glue for
//! the developer's own `npm install` (the BYO model). These drive the compiled
//! binary in tempdirs; a WASM build emits bytes + glue + package.json with no
//! external toolchain (no wasmtime / runtime lib needed), so they are hermetic.

use std::path::Path;
use std::process::Command;

fn phoenix(cwd: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_phoenix"));
    cmd.current_dir(cwd);
    cmd.env("PHOENIX_HOME", home);
    cmd
}

/// A program binding a named npm module plus (optionally) an undeclared one.
fn program(extra_extern: &str) -> String {
    format!(
        "extern js \"left-pad\" {{\n  function leftPad(s: String, width: Int) -> String\n}}\n\
         {extra_extern}\
         function main() {{\n  print(leftPad(\"4\", 3))\n}}\n"
    )
}

fn write_project(proj: &Path, manifest_js_deps: &str, main_extra_extern: &str) {
    std::fs::write(
        proj.join("phoenix.toml"),
        format!("[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n{manifest_js_deps}"),
    )
    .unwrap();
    std::fs::write(proj.join("main.phx"), program(main_extra_extern)).unwrap();
}

fn build_wasm(proj: &Path, home: &Path, out_wasm: &Path) -> std::process::Output {
    phoenix(proj, home)
        .args(["build", "main.phx", "--target", "wasm32-linear", "-o"])
        .arg(out_wasm)
        .output()
        .expect("run phoenix build")
}

#[test]
fn wasm_build_emits_package_json_from_js_dependencies() {
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write_project(
        proj.path(),
        "[js-dependencies]\nleft-pad = \"^1.3.0\"\n",
        "",
    );
    let out = proj.path().join("dist");
    std::fs::create_dir(&out).unwrap();

    let output = build_wasm(proj.path(), home.path(), &out.join("app.wasm"));
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(out.join("app.wasm").is_file(), "wasm artifact");
    assert!(out.join("app.js").is_file(), "glue sidecar");

    let pkg = std::fs::read_to_string(out.join("package.json")).expect("package.json emitted");
    assert!(pkg.contains("\"type\": \"module\""), "{pkg}");
    assert!(pkg.contains("\"left-pad\": \"^1.3.0\""), "{pkg}");
    // Valid JSON.
    let _: serde_json::Value = serde_json::from_str(&pkg).unwrap();
}

#[test]
fn existing_package_json_is_not_clobbered() {
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write_project(
        proj.path(),
        "[js-dependencies]\nleft-pad = \"^1.3.0\"\n",
        "",
    );
    let out = proj.path().join("dist");
    std::fs::create_dir(&out).unwrap();
    // A developer-owned package.json already in the output dir.
    let sentinel = "{ \"name\": \"my-own\", \"scripts\": { \"start\": \"node app.js\" } }\n";
    std::fs::write(out.join("package.json"), sentinel).unwrap();

    let output = build_wasm(proj.path(), home.path(), &out.join("app.wasm"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = std::fs::read_to_string(out.join("package.json")).unwrap();
    assert_eq!(
        after, sentinel,
        "existing package.json must never be clobbered"
    );
}

#[test]
fn undeclared_named_module_warns_but_build_succeeds() {
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    // Declares left-pad, but the program ALSO binds `chalk`, which is undeclared.
    write_project(
        proj.path(),
        "[js-dependencies]\nleft-pad = \"^1.3.0\"\n",
        "extern js \"chalk\" {\n  function red(s: String) -> String\n}\n",
    );
    let out = proj.path().join("dist");
    std::fs::create_dir(&out).unwrap();

    let output = build_wasm(proj.path(), home.path(), &out.join("app.wasm"));
    assert!(
        output.status.success(),
        "an undeclared module must warn, not fail: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("chalk") && stderr.contains("[js-dependencies]"),
        "expected an undeclared-module warning for `chalk`; stderr:\n{stderr}"
    );
    // The declared module is not warned about.
    assert!(
        !stderr.contains("`extern js \"left-pad\"` names a package not"),
        "declared module must not warn; stderr:\n{stderr}"
    );
}

#[test]
fn no_js_dependencies_writes_no_package_json() {
    // A wasm build with no `[js-dependencies]` (only the ambient/named externs
    // it happens to use) writes no package.json — nothing to install.
    let proj = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    // No [js-dependencies] section at all; the program uses the ambient host only.
    std::fs::write(
        proj.path().join("phoenix.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        proj.path().join("main.phx"),
        "extern js {\n  function log(s: String)\n}\nfunction main() {\n  log(\"hi\")\n}\n",
    )
    .unwrap();
    let out = proj.path().join("dist");
    std::fs::create_dir(&out).unwrap();

    let output = build_wasm(proj.path(), home.path(), &out.join("app.wasm"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !out.join("package.json").exists(),
        "no js deps ⇒ no package.json"
    );
}

#[test]
fn dependency_packages_own_extern_js_is_not_the_root_projects_concern() {
    // `app` depends (via a local `path` source) on `dep`, and `dep` — not
    // `app` — declares `extern js "chalk"`. `app` never writes `extern js
    // "chalk"` itself and has no `[js-dependencies]` section at all, so a
    // warning telling the `app` developer to add `chalk` to *their*
    // `phoenix.toml` would be pointing at the wrong manifest — `chalk` is
    // `dep`'s concern, not `app`'s. Regression test for the sema merge
    // (`analysis.module.extern_functions` spans every resolved module,
    // dependency packages included) leaking into the undeclared-js-dependency
    // diagnostic.
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    std::fs::create_dir_all(root.path().join("dep")).unwrap();
    std::fs::write(
        root.path().join("dep/phoenix.toml"),
        "[package]\nname = \"dep\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("dep/mod.phx"),
        "extern js \"chalk\" {\n  function red(s: String) -> String\n}\n\
         public function value() -> String { \"x\" }\n",
    )
    .unwrap();

    std::fs::create_dir_all(root.path().join("app")).unwrap();
    std::fs::write(
        root.path().join("app/phoenix.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\ndep = { path = \"../dep\" }\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("app/main.phx"),
        "import dep { value }\nfunction main() { print(value()) }\n",
    )
    .unwrap();
    let out = root.path().join("app/dist");
    std::fs::create_dir(&out).unwrap();

    let output = build_wasm(&root.path().join("app"), home.path(), &out.join("app.wasm"));
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("chalk"),
        "a dependency package's own `extern js` must not surface as an \
         undeclared-js-dependency warning against the root project; stderr:\n{stderr}"
    );
    // No `[js-dependencies]` anywhere in `app`'s own manifest ⇒ no package.json.
    assert!(
        !out.join("package.json").exists(),
        "app declares no js-dependencies of its own"
    );
}
