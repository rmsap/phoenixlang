//! `black`-style layout primitives for the Python generator.
//!
//! These helpers know nothing about Phoenix semantics; each takes already
//! rendered fragments and emits them wrapped the way `black --check` expects
//! (line length [`LINE_LENGTH`], collapsed continuation lines, exploded
//! signatures with a magic trailing comma, vertical blank-line spacing). They
//! are split out of `python.rs` so the generator there stays focused on *what*
//! to emit rather than *how* to wrap it.

/// Black's default line length. Emitted statements that would exceed this on a
/// single line are wrapped to match `black --check`.
pub(crate) const LINE_LENGTH: usize = 88;

/// Formats a function call the way `black` would. `prefix` is the text up to
/// and including the callable (e.g. `"response = await self.client.get"`),
/// emitted at `base_indent`. `suffix` is appended after the closing paren on
/// whichever line carries it (e.g. `" from e"` for a `raise … from e`; `""` for
/// a plain call). Tries, in order: the whole call on one line; the arguments
/// collapsed onto a single continuation line inside the parens; one argument per
/// line with a magic trailing comma.
pub(crate) fn format_call(
    base_indent: &str,
    prefix: &str,
    args: &[String],
    suffix: &str,
) -> String {
    let one_line = format!("{base_indent}{prefix}({}){suffix}", args.join(", "));
    if one_line.len() <= LINE_LENGTH {
        return format!("{one_line}\n");
    }

    let inner_indent = format!("{base_indent}    ");
    let collapsed = format!("{inner_indent}{}", args.join(", "));
    if collapsed.len() <= LINE_LENGTH {
        return format!("{base_indent}{prefix}(\n{collapsed}\n{base_indent}){suffix}\n");
    }

    let mut out = format!("{base_indent}{prefix}(\n");
    for a in args {
        out.push_str(&format!("{inner_indent}{a},\n"));
    }
    out.push_str(&format!("{base_indent}){suffix}\n"));
    out
}

/// Renders a model field annotated with a Pydantic `Field(<sentinel>, <kwargs>)`
/// default, wrapping the call across lines the way black does once the
/// single-line form exceeds the line length. `sentinel` is the positional first
/// argument: `...` for a required field, `None` for an optional one.
pub(crate) fn pydantic_field_line(
    name: &str,
    ty: &str,
    sentinel: &str,
    kwargs: &[String],
) -> String {
    let mut args = vec![sentinel.to_string()];
    args.extend(kwargs.iter().cloned());
    format_call("    ", &format!("{name}: {ty} = Field"), &args, "")
}

/// Formats a `from <module> import <names>` statement the way `black` would:
/// one line if it fits in the line length, otherwise a parenthesized block with
/// one name per line and a magic trailing comma.
pub(crate) fn format_from_import(module: &str, names: &[String]) -> String {
    let one_line = format!("from {module} import {}", names.join(", "));
    if one_line.len() <= LINE_LENGTH {
        return format!("{one_line}\n");
    }
    let mut out = format!("from {module} import (\n");
    for n in names {
        out.push_str(&format!("    {n},\n"));
    }
    out.push_str(")\n");
    out
}

/// Formats a function-definition signature the way `black` would, so generated
/// output passes `black --check` without reformatting.
///
/// `base_indent` is the indentation of the `def` line (e.g. `"    "`). `params`
/// are the individual parameters (already rendered, e.g. `"page: int = 1"`).
/// `suffix` is everything after the closing paren, e.g. `" -> Post:"` or
/// `" -> Post: ..."`.
///
/// If the one-line form fits in the line length it is emitted as-is; otherwise
/// black explodes each parameter onto its own line with a magic trailing comma.
pub(crate) fn format_def_signature(
    base_indent: &str,
    def_keyword: &str,
    name: &str,
    params: &[String],
    suffix: &str,
) -> String {
    let one_line = format!(
        "{base_indent}{def_keyword} {name}({}){suffix}",
        params.join(", ")
    );
    if one_line.len() <= LINE_LENGTH {
        return format!("{one_line}\n");
    }

    let param_indent = format!("{base_indent}    ");
    let mut out = format!("{base_indent}{def_keyword} {name}(\n");
    for p in params {
        out.push_str(&format!("{param_indent}{p},\n"));
    }
    out.push_str(&format!("{base_indent}){suffix}\n"));
    out
}

/// Ensures `buf` ends with exactly `blank_lines` blank lines before the next
/// emitted item, matching black's vertical-spacing rules (2 blank lines between
/// top-level defs, 1 between methods). Trailing whitespace-only lines are
/// trimmed first so the spacing is exact regardless of prior content.
pub(crate) fn ensure_blank_lines(buf: &mut String, blank_lines: usize) {
    while buf.ends_with('\n') {
        buf.pop();
    }
    if buf.is_empty() {
        return;
    }
    // The first statement inside a freshly opened block (the previous line ends
    // with `:`) gets no leading blank line, matching black (e.g. a method right
    // after `class X:`).
    let opens_block = buf
        .rsplit('\n')
        .next()
        .map(|line| line.trim_end().ends_with(':'))
        .unwrap_or(false);
    let blanks = if opens_block { 0 } else { blank_lines };
    for _ in 0..=blanks {
        buf.push('\n');
    }
}
