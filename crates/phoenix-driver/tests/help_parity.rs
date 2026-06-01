//! Parity test: the standalone `phoenix-gen` binary must expose the
//! same option surface as the `phoenix gen` subcommand. The two help texts
//! legitimately differ in the usage / program-name line and in clap's
//! auto-generated meta flags (`--help` / `--version`), so we compare the set of
//! user-defined long options after stripping those.

use std::collections::BTreeSet;
use std::process::Command;

/// Extract the set of user-defined long-option flags (e.g. `--target`, `--out`)
/// from `--help` output.
///
/// clap renders options in an indented block; a single option line may lead
/// with a short alias (`  -o, --out <OUT>`), so we collect *every* `--word`
/// token on each indented line rather than just the first token — otherwise an
/// aliased option like `--out` would be missed. Tokens are normalized by
/// trimming the trailing punctuation clap attaches (`,`, `=`, `<...>`).
///
/// clap's auto-generated `--help` and `--version` are excluded: the subcommand
/// help shows only `--help`, while the standalone binary (a top-level
/// `Parser`) also shows `--version`. Those meta flags are not part of the gen
/// option surface under test.
fn options(help: &str) -> BTreeSet<String> {
    let mut opts = BTreeSet::new();
    for line in help.lines() {
        // Only the indented options block; the usage line is not indented.
        if !line.starts_with(char::is_whitespace) {
            continue;
        }
        for token in line.split_whitespace() {
            if let Some(rest) = token.strip_prefix("--") {
                // Cut at the first non-flag char (e.g. `--out=<OUT>` -> `out`).
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '-')
                    .collect();
                if name.is_empty() {
                    continue;
                }
                let flag = format!("--{name}");
                if flag != "--help" && flag != "--version" {
                    opts.insert(flag);
                }
            }
        }
    }
    opts
}

fn run_help(exe: &str, args: &[&str]) -> String {
    let out = Command::new(exe)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {exe} {args:?}: {e}"));
    assert!(
        out.status.success(),
        "{exe} {args:?} exited with failure:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn gen_help_options_match_phoenix_gen() {
    // cargo sets CARGO_BIN_EXE_<name> for every bin target of the crate under test.
    let phoenix = env!("CARGO_BIN_EXE_phoenix");
    let phoenix_gen = env!("CARGO_BIN_EXE_phoenix-gen");

    let sub_opts = options(&run_help(phoenix, &["gen", "--help"]));
    let standalone_opts = options(&run_help(phoenix_gen, &["--help"]));

    // Guard against a vacuous test: the known gen options must all be present.
    for expected in ["--target", "--out", "--client", "--server", "--watch"] {
        assert!(
            sub_opts.contains(expected),
            "`phoenix gen --help` is missing {expected}; got {sub_opts:?}"
        );
    }

    assert_eq!(
        sub_opts, standalone_opts,
        "option surfaces differ:\n  phoenix gen: {sub_opts:?}\n  phoenix-gen: {standalone_opts:?}"
    );
}
