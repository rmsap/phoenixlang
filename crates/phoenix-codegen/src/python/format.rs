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

/// Renders a single pydantic model field line for a model whose wire format is the
/// schema's original (camelCase) names — the cross-language-compatible form shared
/// with the Go (`json:"…"`) and TS targets. `py_name` is the snake_case Python
/// attribute, `wire` the schema field name. When they differ (a camelCase field, or
/// a keyword escaped with a trailing `_`), a `Field(alias="<wire>")` keeps the wire
/// key as the schema name (the model needs `populate_by_name` so it still
/// constructs by `py_name`); otherwise the plain `name: type[ = None]` form is kept
/// so single-word fields don't churn. `where`-constraints are NOT rendered here:
/// they are enforced by a `@model_validator` over the FULL expression (see
/// `python.rs::emit_constraints_validator`), matching Go/TS rather than the old
/// extractable-only `Field(...)` kwargs.
///
/// Callers MUST pass `to_snake_case(<wire>)` as `py_name`, so the `py_name != wire`
/// test here is exactly `python.rs::is_aliased(wire)` — the single alias predicate
/// the import-gating scan and `model_config` emission also use. (This module stays
/// free of Phoenix semantics, so it can't call `is_aliased` directly.)
pub(crate) fn pydantic_model_field(
    py_name: &str,
    ty: &str,
    is_optional: bool,
    wire: &str,
) -> String {
    let mut kwargs: Vec<String> = Vec::new();
    if py_name != wire {
        kwargs.push(format!("alias=\"{wire}\""));
    }
    if kwargs.is_empty() {
        if is_optional {
            format!("    {py_name}: {ty} = None\n")
        } else {
            format!("    {py_name}: {ty}\n")
        }
    } else {
        let sentinel = if is_optional { "None" } else { "..." };
        pydantic_field_line(py_name, ty, sentinel, &kwargs)
    }
}

/// Formats a `from <module> import <names>` statement the way `black` would:
/// one line if it fits in the line length, otherwise a parenthesized block with
/// one name per line and a magic trailing comma.
///
/// `names` are re-sorted case-insensitively to match isort (ruff's `I` rules),
/// which orders e.g. `Reaction` before `ReactToPostBody` (comparing "reacti…" <
/// "reactt…"). The callers collect names through a `BTreeSet`, whose ASCII order
/// instead puts the uppercase `T` first — which `ruff check` flags as an unsorted
/// import block (I001). All callers pass `.models` imports (PascalCase class
/// names), so a plain lowercase key matches isort's `order-by-type` grouping too.
pub(crate) fn format_from_import(module: &str, names: &[String]) -> String {
    let mut names: Vec<&str> = names.iter().map(String::as_str).collect();
    // Case-insensitive primary key, original ASCII as the tiebreaker (so a casing
    // difference alone is still ordered deterministically). `sort_by_cached_key`
    // lowercases each name once rather than on every comparison.
    names.sort_by_cached_key(|n| (n.to_lowercase(), *n));

    let one_line = format!("from {module} import {}", names.join(", "));
    if one_line.len() <= LINE_LENGTH {
        return format!("{one_line}\n");
    }
    let mut out = format!("from {module} import (\n");
    for n in &names {
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
        let line = format!("{param_indent}{p},");
        if line.len() <= LINE_LENGTH {
            out.push_str(&line);
            out.push('\n');
        } else {
            // The param itself overflows. Black, for a `lhs = Call(args)`
            // parameter, breaks *inside* the call: the call's args go on an
            // indented continuation line and the `)` closes at the param's own
            // indent. The ONLY over-long param the generator emits is a FastAPI
            // `Query(...)` default (a long camelCase alias), which has exactly
            // this shape, so `wrap_call_param` handles it. A hypothetical
            // non-call param over the limit (e.g. a 90-col bare type annotation)
            // would fall back to the unwrapped line — still valid Python, but
            // `black --check` in the compile-and-lint suite would reject it. The
            // generator produces no such param today; if that ever changes, this
            // branch needs a general black-style line wrapper, not just the call
            // form.
            out.push_str(&wrap_call_param(&param_indent, p));
        }
    }
    out.push_str(&format!("{base_indent}){suffix}\n"));
    out
}

/// Wraps a single over-long function parameter of the form
/// `lhs = Callee(inner)` the way black does: `lhs = Callee(` on the first line,
/// `inner` indented one level deeper, and `)` back at the parameter's indent —
/// followed by the trailing comma. Falls back to the param unchanged if it does
/// not match that shape.
fn wrap_call_param(param_indent: &str, param: &str) -> String {
    if let Some(open) = param.find('(')
        && param.ends_with(')')
    {
        let head = &param[..open]; // e.g. `x: int = Query`
        let inner = &param[open + 1..param.len() - 1]; // call args
        let inner_indent = format!("{param_indent}    ");
        return format!("{param_indent}{head}(\n{inner_indent}{inner}\n{param_indent}),\n");
    }
    format!("{param_indent}{param},\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An over-long `lhs = Callee(args)` param wraps black-style: the call opens
    /// on the param's line, its args drop to a deeper indent, and `)` closes back
    /// at the param indent, followed by the trailing comma.
    #[test]
    fn wrap_call_param_breaks_inside_the_call() {
        let wrapped = wrap_call_param(
            "        ",
            "sort_by: str = Query(\"default\", alias=\"sortBy\")",
        );
        assert_eq!(
            wrapped,
            "        sort_by: str = Query(\n            \"default\", alias=\"sortBy\"\n        ),\n"
        );
    }

    /// A param with no `Callee(...)` shape (no parens) can't be re-wrapped, so it
    /// falls back to the unwrapped single line — still valid Python.
    #[test]
    fn wrap_call_param_falls_back_without_call_shape() {
        let wrapped = wrap_call_param("        ", "body: SomeVeryLongRequestBodyTypeName");
        assert_eq!(wrapped, "        body: SomeVeryLongRequestBodyTypeName,\n");
    }

    /// `format_def_signature` must route an individually-overflowing `Query(...)`
    /// param through `wrap_call_param`, not emit it as one >88-col line. Exercises
    /// the overflow branch end-to-end (the generator's real param lines are all
    /// short, so this is the only coverage of that path).
    #[test]
    fn format_def_signature_wraps_overflowing_query_param() {
        let param = "extremely_long_query_parameter_name: str = \
             Query(\"some_default\", alias=\"extremelyLongQueryParameterName\")"
            .to_string();
        let sig = format_def_signature(
            "    ",
            "async def",
            "list_things",
            std::slice::from_ref(&param),
            " -> None:",
        );
        // The call opened and broke (args dropped to a continuation line)...
        assert!(
            sig.contains("= Query(\n"),
            "overflowing Query param should break inside the call:\n{sig}"
        );
        // ...and `)` closed back at the 8-col param indent with the trailing comma.
        assert!(
            sig.contains("\n        ),\n"),
            "wrapped call should close `)` at the param indent:\n{sig}"
        );
        // The param must NOT survive as one unwrapped >88-col line.
        assert!(
            !sig.contains(&format!("{param},")),
            "overflowing param should not be emitted on a single line:\n{sig}"
        );
    }

    /// Import names are ordered case-insensitively to match isort (ruff's `I`
    /// rules): `Reaction` precedes `ReactToPostBody` (comparing "reacti…" <
    /// "reactt…"), where a `BTreeSet`'s ASCII order would put the uppercase `T`
    /// first and trip ruff I001. Regression for the un-sorted-import-block gap.
    #[test]
    fn format_from_import_orders_names_case_insensitively() {
        // Passed in ASCII (BTreeSet) order, which puts `ReactTo…` before `Reaction`.
        let names = [
            "PublicProfile".to_string(),
            "ReactToPostBody".to_string(),
            "Reaction".to_string(),
            "SearchUsersPage".to_string(),
        ];
        assert_eq!(
            format_from_import(".models", &names),
            "from .models import PublicProfile, Reaction, ReactToPostBody, SearchUsersPage\n"
        );
    }
}
