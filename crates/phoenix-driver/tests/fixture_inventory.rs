//! Every single-file fixture — a `.phx` at the top of `tests/fixtures/` or
//! in `tests/fixtures/poisoned/` — must be claimed by a test suite: its
//! filename must appear in at least one `.rs` file under `crates/`.
//!
//! Multi-module fixture TREES (`multi/`, `multi_negative/`,
//! `multi_module_*/`) are deliberately out of scope: the module-system
//! suites exercise them as whole directories, and their per-file names
//! (`main.phx`, `lib.phx`, …) are generic and never cited individually, so
//! a by-filename scan over them would only produce false orphans.
//!
//! The suites that consume fixtures (`backend_matrix.rs`,
//! `gen_schema_fixtures.rs`, `gen_cli.rs`, …) each keep a manually
//! maintained list, and the split between them is a comment-level
//! contract — nothing else fails when a new fixture lands without being
//! added to the right list; it just silently goes unguarded. This scan
//! turns that omission into a test failure that names the orphan. For the
//! realistic gen schema library specifically, the contract is tighter than
//! "claimed somewhere": each schema must appear in BOTH guarding lists, so
//! [`gen_schema_library_lists_match`] additionally asserts those two lists
//! name the same fixtures.
//!
//! "Referenced by filename" is deliberately loose (a mention in a comment
//! counts): the failure mode being guarded is *forgetting a new fixture
//! entirely*, not misclassifying one. Anyone citing a fixture by name in
//! source is expected to also be wiring or excluding it deliberately.
//! The one tightening: the mention must sit at an identifier boundary, so
//! a reference to `file_storage.phx` can't silently claim a future
//! `storage.phx` (see [`claims`]).

mod common;

use common::compiled_fixtures::workspace_root;
use std::path::Path;

/// Directories that contain `.rs` files which can never be a fixture's
/// claim site (build output, vendored toolchains, generated code).
const SKIP_DIRS: &[&str] = &["target", "node_modules", ".venv", "generated"];

fn collect_rust_sources(dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read source dir") {
        let path = entry.expect("read dir entry").path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if !SKIP_DIRS.contains(&name.as_ref()) {
                collect_rust_sources(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(std::fs::read_to_string(&path).expect("read .rs file"));
        }
    }
}

/// Collect the `.phx` filenames directly inside `dir` (no recursion — the
/// multi-module trees are excluded by design; see the module doc).
fn collect_fixture_names(dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read fixtures dir") {
        let path = entry.expect("read fixture entry").path();
        if path.extension().is_some_and(|e| e == "phx") {
            out.push(
                path.file_name()
                    .expect("fixture has a filename")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
}

/// True when `c` (the character adjacent to a candidate match, or `None` at
/// the start/end of the source) cannot extend an identifier — i.e. the match
/// sits at an identifier boundary on that side.
fn at_boundary(c: Option<char>) -> bool {
    !c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// True when `src` mentions `name` at identifier boundaries. Plain substring
/// containment would let any fixture claim another whose name is a suffix of
/// its own (`file_storage.phx` claiming a future `storage.phx`) — or, on the
/// trailing side, a longer mention like `social.phxbackup` claim `social.phx`
/// — recreating the silent-orphan hole this test exists to close. Requiring a
/// non-identifier character (quote, `/`, backtick, space, …) on both sides
/// blocks the shadowing while keeping comment mentions valid as claims.
fn claims(src: &str, name: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = src[from..].find(name) {
        let at = from + pos;
        let prev = src[..at].chars().next_back();
        let next = src[at + name.len()..].chars().next();
        if at_boundary(prev) && at_boundary(next) {
            return true;
        }
        from = at + 1;
    }
    false
}

#[test]
fn every_fixture_is_claimed_by_a_suite() {
    let root = workspace_root();
    let mut sources = Vec::new();
    collect_rust_sources(&root.join("crates"), &mut sources);

    let mut names = Vec::new();
    collect_fixture_names(&root.join("tests/fixtures"), &mut names);
    collect_fixture_names(&root.join("tests/fixtures/poisoned"), &mut names);

    let mut orphans = Vec::new();
    for name in names {
        if !sources.iter().any(|src| claims(src, &name)) {
            orphans.push(name);
        }
    }

    orphans.sort();
    assert!(
        orphans.is_empty(),
        "fixtures not referenced by any .rs file under crates/ — wire each \
         into the suite that should guard it (the three-backend matrix for \
         runnable programs, gen_schema_fixtures for gen schemas, …): {orphans:?}"
    );
}

// ── Gen schema library: the two guarding lists must match ─────────────────

/// Extract the section of `src` between `start_marker` and the next
/// `end_marker` after it, panicking (with `what` for attribution) if either
/// is missing — a marker disappearing means the list this test parses was
/// restructured, and the test must be updated rather than silently pass.
fn section<'a>(src: &'a str, start_marker: &str, end_marker: &str, what: &str) -> &'a str {
    let start = src
        .find(start_marker)
        .unwrap_or_else(|| panic!("{what}: marker {start_marker:?} not found"));
    let rest = &src[start..];
    let len = rest
        .find(end_marker)
        .unwrap_or_else(|| panic!("{what}: end marker {end_marker:?} not found"));
    &rest[..len]
}

/// Collect the basenames of every `"….phx"` string literal in `block`
/// (an `include_str!` path and its tuple-key name reduce to the same
/// basename, so the two entry styles need no special-casing).
fn phx_literal_basenames(block: &str) -> std::collections::BTreeSet<String> {
    block
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|lit| lit.ends_with(".phx"))
        .map(|lit| {
            lit.rsplit('/')
                .next()
                .expect("rsplit yields a piece")
                .to_owned()
        })
        .collect()
}

/// The realistic gen schema library is guarded by two hand-maintained lists:
/// `FILE_FIXTURES` in `phoenix-codegen`'s `compiles_and_lints.rs` (the gated
/// compile-and-lint loops) and the `schema_fixture_checks!` invocation in
/// `gen_schema_fixtures.rs` (the always-on `phoenix check` gate). The orphan
/// scan above only proves a fixture is claimed by *some* list; this test
/// closes the remaining hole — a schema added to one list but forgotten in
/// the other would otherwise silently skip half its coverage.
#[test]
fn gen_schema_library_lists_match() {
    let root = workspace_root();
    let read = |rel: &str| {
        std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
    };

    let checks_src = read("crates/phoenix-driver/tests/gen_schema_fixtures.rs");
    let lints_src = read("crates/phoenix-codegen/tests/compiles_and_lints.rs");

    let checked = phx_literal_basenames(section(
        &checks_src,
        "schema_fixture_checks! {",
        "}",
        "gen_schema_fixtures.rs",
    ));
    let linted = phx_literal_basenames(section(
        &lints_src,
        "const FILE_FIXTURES",
        "];",
        "compiles_and_lints.rs",
    ));

    let only_checked: Vec<_> = checked.difference(&linted).collect();
    let only_linted: Vec<_> = linted.difference(&checked).collect();
    assert!(
        only_checked.is_empty() && only_linted.is_empty(),
        "the gen schema library's two guarding lists diverged — \
         in gen_schema_fixtures.rs but missing from FILE_FIXTURES \
         (no compile-and-lint coverage): {only_checked:?}; \
         in FILE_FIXTURES but missing from gen_schema_fixtures.rs \
         (no `phoenix check` gate): {only_linted:?}"
    );
}
