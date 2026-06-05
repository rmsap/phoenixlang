//! Prettier-style layout primitives for the TypeScript generator.
//!
//! These helpers know nothing about Phoenix semantics; each takes already
//! rendered fragments and emits them wrapped the way `prettier --check`
//! expects (line width [`PRINT_WIDTH`], leading-`|` unions, broken call
//! arguments, exploded signatures, etc.). They are split out of
//! `typescript.rs` so the generator there stays focused on *what* to emit
//! rather than *how* to wrap it.

use phoenix_sema::types::Type;

/// Prettier's default print width. Emitted statements that would exceed this
/// on a single line are wrapped to match `prettier --check`.
pub(crate) const PRINT_WIDTH: usize = 80;

/// Normalizes a generated file to Prettier's whitespace expectations: no
/// trailing whitespace on any line, exactly one trailing newline, and no blank
/// line at end of file.
pub(crate) fn finalize(src: String) -> String {
    let mut out: String = src
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    // Collapse any trailing blank lines, then add exactly one newline.
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}

/// Emits `prefix(args)suffix` at `indent`, matching Prettier: the whole call on
/// one line if it fits, otherwise each argument on its own line with a magic
/// trailing comma. (Unlike `black`, Prettier has no collapsed-onto-a-single-
/// continuation-line middle form.)
pub(crate) fn emit_call_stmt(
    out: &mut String,
    indent: &str,
    prefix: &str,
    args: &[String],
    suffix: &str,
) {
    let one_line = format!("{indent}{prefix}({}){suffix}", args.join(", "));
    if one_line.len() <= PRINT_WIDTH {
        out.push_str(&one_line);
        out.push('\n');
        return;
    }
    out.push_str(&format!("{indent}{prefix}(\n"));
    for a in args {
        out.push_str(&format!("{indent}  {a},\n"));
    }
    out.push_str(&format!("{indent}){suffix}\n"));
}

/// Emits an Express route's `async (req: T, res: Response) => {` arrow header at
/// `indent`, breaking the parameter list across lines (Prettier style) when the
/// single-line form overflows — e.g. when `T` is a wide multi-param
/// `Request<{…}>`. Used only in the wrapped-route layout.
pub(crate) fn emit_arrow_header(out: &mut String, indent: &str, req_type: &str) {
    let one_line = format!("{indent}async (req: {req_type}, res: Response) => {{");
    if one_line.len() <= PRINT_WIDTH {
        out.push_str(&one_line);
        out.push('\n');
    } else {
        out.push_str(&format!("{indent}async (\n"));
        out.push_str(&format!("{indent}  req: {req_type},\n"));
        out.push_str(&format!("{indent}  res: Response,\n"));
        out.push_str(&format!("{indent}) => {{\n"));
    }
}

/// Emits an object property `key: value,`, wrapping to Prettier's layout when
/// the single-line form overflows. Prettier's choices, in the order it tries
/// them (and that this function mirrors):
///   * `key: value,` on one line, if it fits;
///   * `EXPR as A | B | …` → `key: EXPR as` then one leading-`|` union member
///     per line. Prettier prefers this over dropping the cast to its own line,
///     so it is checked *before* the value-on-own-line case even when the value
///     would fit on its own line;
///   * `key:` then the whole value on its own indented line, if it fits there
///     (this covers a ternary that fits once indented, e.g. `minScore`);
///   * a still-too-wide ternary `cond ? a : b` → `key:`, then the condition,
///     then the `?`/`:` arms each on their own line;
///   * otherwise, the value on its own indented line as a last resort.
///
/// The last resort assumes a value with no further internal break Prettier
/// would prefer (e.g. a member chain like `req.query.x as string`, which
/// Prettier breaks at the `.`). Coercion never emits such a value wide enough to
/// reach it; if a new coercion form is added, verify its overflow layout against
/// Prettier.
pub(crate) fn emit_object_property(out: &mut String, indent: &str, key: &str, value: &str) {
    let one_line = format!("{indent}{key}: {value},");
    if one_line.len() <= PRINT_WIDTH {
        out.push_str(&one_line);
        out.push('\n');
        return;
    }

    // `EXPR as A | B`: keep `key: EXPR as` on the first line, then break the
    // union type with a leading `|` per member (the final member takes the
    // property's trailing comma). Wins over value-on-own-line.
    if let Some((expr, members)) = split_as_union(value) {
        out.push_str(&format!("{indent}{key}: {expr} as\n"));
        for (i, m) in members.iter().enumerate() {
            let comma = if i + 1 == members.len() { "," } else { "" };
            out.push_str(&format!("{indent}  | {m}{comma}\n"));
        }
        return;
    }

    // The whole value on its own indented line, if it fits there.
    let value_line = format!("{indent}  {value},");
    if value_line.len() <= PRINT_WIDTH {
        out.push_str(&format!("{indent}{key}:\n{value_line}\n"));
        return;
    }

    // A ternary too wide even on its own line: break it across its `?`/`:` arms.
    if let Some((cond, a, b)) = split_ternary(value) {
        out.push_str(&format!(
            "{indent}{key}:\n{indent}  {cond}\n{indent}    ? {a}\n{indent}    : {b},\n"
        ));
        return;
    }

    // Last resort: value on its own indented line. Identical emission to the
    // fits-on-own-line case above, but reached when the value is still over
    // width and Prettier has no further break it prefers (see the doc comment).
    out.push_str(&format!("{indent}{key}:\n{value_line}\n"));
}

/// Splits `EXPR as A | B | …` into (`EXPR`, `[A, B, …]`) when the cast targets a
/// union type. Returns `None` when there is no ` as ` or the cast type is a
/// single (non-union) type. Mirrors Prettier's leading-`|` break of a long
/// union `as` cast. Coercion never nests ` as ` inside `EXPR`, so the first
/// ` as ` is the cast operator.
pub(crate) fn split_as_union(value: &str) -> Option<(&str, Vec<&str>)> {
    let pos = value.find(" as ")?;
    let expr = &value[..pos];
    let members: Vec<&str> = value[pos + 4..].split(" | ").collect();
    if members.len() > 1 {
        Some((expr, members))
    } else {
        None
    }
}

/// Splits a top-level ternary `cond ? a : b` into its three parts. Only handles
/// the un-nested ternaries produced by query-parameter coercion, whose `a` arm
/// (a coercion expression) never contains `" : "`. We therefore take the FIRST
/// `" : "` after the `?` as the separator, so a string default in `b` that
/// itself contains `" : "` can't be mistaken for it.
pub(crate) fn split_ternary(expr: &str) -> Option<(&str, &str, &str)> {
    let q = expr.find(" ? ")?;
    let rest = &expr[q + 3..];
    let c_rel = rest.find(" : ")?;
    let c = q + 3 + c_rel;
    Some((&expr[..q], &expr[q + 3..c], &expr[c + 3..]))
}

/// Emits `if (cond) { stmt; ... }` always expanded across lines, which is how
/// Prettier formats a guarded block containing more than one statement.
pub(crate) fn emit_guarded_block(out: &mut String, indent: &str, cond: &str, stmts: &[String]) {
    out.push_str(&format!("{indent}if ({cond}) {{\n"));
    for s in stmts {
        out.push_str(&format!("{indent}  {s}\n"));
    }
    out.push_str(&format!("{indent}}}\n"));
}

/// Emits `if (cond) stmt`, dropping `stmt` onto an indented next line when the
/// single-line form exceeds the print width (Prettier's layout for a single
/// guarded statement).
pub(crate) fn emit_if_stmt(out: &mut String, indent: &str, cond: &str, stmt: &str) {
    let one_line = format!("{indent}if ({cond}) {stmt}");
    if one_line.len() <= PRINT_WIDTH {
        out.push_str(&one_line);
        out.push('\n');
    } else {
        out.push_str(&format!("{indent}if ({cond})\n{indent}  {stmt}\n"));
    }
}

/// Emits a query param's `params.set(...)` line for the client.
///
/// A **required** param (`optional == false`) is always present on a
/// non-nullable `opts`, so it is set unconditionally — emitting a
/// `!== undefined` guard or `opts?.` chain there trips eslint's
/// `no-unnecessary-condition`. An **optional** param keeps the
/// `if (… !== undefined)` guard; the access uses `opts?.` only when `opts`
/// itself is nullable (`opts_nullable`, i.e. every param is optional), and a
/// plain `opts.` otherwise (a redundant `?.` on a non-nullable `opts` would also
/// trip eslint).
///
/// Matches Prettier's layouts as the line lengthens: everything on one line; the
/// `params.set(...)` call dropped onto the next line; then the call broken one
/// argument per line with a magic trailing comma. (The condition is short for
/// any realistic field name, so its own overflow layout is not emulated.)
pub(crate) fn emit_param_set(
    out: &mut String,
    indent: &str,
    name: &str,
    optional: bool,
    opts_nullable: bool,
) {
    let arg0 = format!("\"{name}\"");
    let arg1 = format!("String(opts.{name})");

    if !optional {
        // Required: opts is non-nullable and the field is always present.
        let one_line = format!("{indent}params.set({arg0}, {arg1});");
        if one_line.len() <= PRINT_WIDTH {
            out.push_str(&one_line);
            out.push('\n');
        } else {
            out.push_str(&format!("{indent}params.set(\n"));
            out.push_str(&format!("{indent}  {arg0},\n"));
            out.push_str(&format!("{indent}  {arg1},\n"));
            out.push_str(&format!("{indent});\n"));
        }
        return;
    }

    let access = if opts_nullable {
        format!("opts?.{name}")
    } else {
        format!("opts.{name}")
    };
    let cond = format!("{access} !== undefined");

    let one_line = format!("{indent}if ({cond}) params.set({arg0}, {arg1});");
    if one_line.len() <= PRINT_WIDTH {
        out.push_str(&one_line);
        out.push('\n');
        return;
    }

    out.push_str(&format!("{indent}if ({cond})\n"));
    let stmt_indent = format!("{indent}  ");
    let set_line = format!("{stmt_indent}params.set({arg0}, {arg1});");
    if set_line.len() <= PRINT_WIDTH {
        out.push_str(&set_line);
        out.push('\n');
    } else {
        out.push_str(&format!("{stmt_indent}params.set(\n"));
        out.push_str(&format!("{stmt_indent}  {arg0},\n"));
        out.push_str(&format!("{stmt_indent}  {arg1},\n"));
        out.push_str(&format!("{stmt_indent});\n"));
    }
}

/// Emits a request header's `requestHeaders.set(...)` line for the client.
///
/// `target` is the `Headers` instance variable (`requestHeaders`); `local` is
/// the camelCase field on the `headers` param the value is read from; `wire` is
/// the exact HTTP header name. The value is stringified via `String(...)`.
///
/// A **required** header is set unconditionally (the `headers` param is
/// non-nullable and the field is always present); emitting a `!== undefined`
/// guard or `headers?.` chain there would trip eslint's
/// `no-unnecessary-condition`. An **optional** header keeps the
/// `if (… !== undefined)` guard, accessing via `headers?.` only when the
/// `headers` param itself is nullable (every header optional) and a plain
/// `headers.` otherwise. Matches Prettier's wrapping as the line lengthens.
pub(crate) fn emit_header_set(
    out: &mut String,
    indent: &str,
    target: &str,
    local: &str,
    wire: &str,
    optional: bool,
    headers_nullable: bool,
) {
    let arg0 = format!("\"{wire}\"");
    let arg1 = format!("String(headers.{local})");

    if !optional {
        let one_line = format!("{indent}{target}.set({arg0}, {arg1});");
        if one_line.len() <= PRINT_WIDTH {
            out.push_str(&one_line);
            out.push('\n');
        } else {
            out.push_str(&format!("{indent}{target}.set(\n"));
            out.push_str(&format!("{indent}  {arg0},\n"));
            out.push_str(&format!("{indent}  {arg1},\n"));
            out.push_str(&format!("{indent});\n"));
        }
        return;
    }

    let access = if headers_nullable {
        format!("headers?.{local}")
    } else {
        format!("headers.{local}")
    };
    let cond = format!("{access} !== undefined");

    let one_line = format!("{indent}if ({cond}) {target}.set({arg0}, {arg1});");
    if one_line.len() <= PRINT_WIDTH {
        out.push_str(&one_line);
        out.push('\n');
        return;
    }

    out.push_str(&format!("{indent}if ({cond})\n"));
    let stmt_indent = format!("{indent}  ");
    let set_line = format!("{stmt_indent}{target}.set({arg0}, {arg1});");
    if set_line.len() <= PRINT_WIDTH {
        out.push_str(&set_line);
        out.push('\n');
    } else {
        out.push_str(&format!("{stmt_indent}{target}.set(\n"));
        out.push_str(&format!("{stmt_indent}  {arg0},\n"));
        out.push_str(&format!("{stmt_indent}  {arg1},\n"));
        out.push_str(&format!("{stmt_indent});\n"));
    }
}

/// Emits `const response = await fetch(url, { ...init });`, matching Prettier's
/// two layouts: the init object's braces break in place when the opening
/// `fetch(url, {` line fits, otherwise the whole call breaks one argument per
/// line.
pub(crate) fn emit_fetch_call(out: &mut String, indent: &str, url: &str, init_lines: &[String]) {
    let open = format!("{indent}const response = await fetch({url}, {{");
    if open.len() <= PRINT_WIDTH {
        out.push_str(&open);
        out.push('\n');
        for line in init_lines {
            out.push_str(&format!("{indent}  {line},\n"));
        }
        out.push_str(&format!("{indent}}});\n"));
    } else {
        out.push_str(&format!("{indent}const response = await fetch(\n"));
        out.push_str(&format!("{indent}  {url},\n"));
        out.push_str(&format!("{indent}  {{\n"));
        for line in init_lines {
            out.push_str(&format!("{indent}    {line},\n"));
        }
        out.push_str(&format!("{indent}  }},\n"));
        out.push_str(&format!("{indent});\n"));
    }
}

/// A function/method parameter, used by the signature formatter to mirror
/// Prettier's wrapping decisions.
pub(crate) enum Param {
    /// A plain `name: type` (or `name?: type`) parameter.
    Simple(String),
    /// A parameter whose type is an inline object literal, e.g.
    /// `opts?: { a: T; b: T }`. The object braces break independently when the
    /// parameter is the sole overflowing argument.
    Object {
        /// The leading `name:` or `name?:` text (including the trailing space).
        prefix: String,
        /// The object's `field: type` members.
        fields: Vec<String>,
    },
}

impl Param {
    /// Renders the parameter on a single line.
    fn inline(&self) -> String {
        match self {
            Param::Simple(s) => s.clone(),
            Param::Object { prefix, fields } => {
                format!("{prefix}{{ {} }}", fields.join("; "))
            }
        }
    }
}

/// Emits a function/method signature, choosing among Prettier's layouts based
/// on the print width:
///   * everything on one line;
///   * a single object parameter whose braces expand (members one per line);
///   * the full parameter list broken (one parameter per line, trailing comma).
///
/// `head` is the text before `(` (e.g. `  listPosts` or `  async getPost`),
/// `tail` is the text after `)` (e.g. `: Promise<Post>;` or `: Promise<Post> {`),
/// and `base_indent` is the indentation of the signature's first line.
pub(crate) fn format_signature(
    head: &str,
    params: &[Param],
    tail: &str,
    base_indent: &str,
) -> String {
    let inline_params = params
        .iter()
        .map(Param::inline)
        .collect::<Vec<_>>()
        .join(", ");
    let one_line = format!("{head}({inline_params}){tail}");
    if one_line.len() <= PRINT_WIDTH {
        return format!("{one_line}\n");
    }

    // Single object parameter: expand its braces, keep it as the only arg.
    if params.len() == 1
        && let Param::Object { prefix, fields } = &params[0]
    {
        let mut out = format!("{head}({prefix}{{\n");
        for f in fields {
            out.push_str(&format!("{base_indent}  {f};\n"));
        }
        out.push_str(&format!("{base_indent}}}){tail}\n"));
        return out;
    }

    // Break the parameter list, one parameter per line with a trailing comma.
    // An `Object` param whose own inline line would overflow has its braces
    // expanded in place (members one per line), matching how Prettier breaks a
    // wide object-type parameter inside an already-broken parameter list.
    let mut out = format!("{head}(\n");
    for p in params {
        let inline_line = format!("{base_indent}  {},", p.inline());
        match p {
            Param::Object { prefix, fields } if inline_line.len() > PRINT_WIDTH => {
                out.push_str(&format!("{base_indent}  {prefix}{{\n"));
                for f in fields {
                    out.push_str(&format!("{base_indent}    {f};\n"));
                }
                out.push_str(&format!("{base_indent}  }},\n"));
            }
            _ => {
                out.push_str(&format!("{inline_line}\n"));
            }
        }
    }
    out.push_str(&format!("{base_indent}){tail}\n"));
    out
}

/// Emits an import statement, wrapping the named-import list across lines when
/// the single-line form exceeds the print width (Prettier style).
///
/// `keyword` is `import` or `import type`.
pub(crate) fn emit_import(out: &mut String, keyword: &str, names: &[String], module: &str) {
    let one_line = format!("{keyword} {{ {} }} from \"{module}\";", names.join(", "));
    if one_line.len() <= PRINT_WIDTH {
        out.push_str(&one_line);
        out.push('\n');
    } else {
        out.push_str(&format!("{keyword} {{\n"));
        for n in names {
            out.push_str(&format!("  {n},\n"));
        }
        out.push_str(&format!("}} from \"{module}\";\n"));
    }
}

/// Emits `export type Name = "A" | "B" | ...;`, wrapping to Prettier's
/// leading-`|` multi-line form when the single-line declaration exceeds the
/// print width. Always followed by a blank line.
pub(crate) fn emit_union_type_alias(out: &mut String, name: &str, members: &[String]) {
    let one_line = format!("export type {name} = {};", members.join(" | "));
    if one_line.len() <= PRINT_WIDTH {
        out.push_str(&one_line);
        out.push_str("\n\n");
    } else {
        out.push_str(&format!("export type {name} =\n"));
        for m in members {
            out.push_str(&format!("  | {m}\n"));
        }
        // Replace the final newline with `;` terminator.
        out.pop();
        out.push_str(";\n\n");
    }
}

/// Emits the opening line(s) of a `validate{Type}` function, wrapping the
/// single `input: unknown` parameter onto its own line (Prettier style) when
/// the one-line signature exceeds the print width.
pub(crate) fn emit_validate_signature(out: &mut String, type_name: &str) {
    let one_line = format!("export function validate{type_name}(input: unknown): {type_name} {{");
    if one_line.len() <= PRINT_WIDTH {
        out.push_str(&one_line);
        out.push('\n');
    } else {
        out.push_str(&format!(
            "export function validate{type_name}(\n  input: unknown,\n): {type_name} {{\n"
        ));
    }
}

/// A normalized description of one field for runtime validation, shared by the
/// struct and derived-body validation emitters.
pub(crate) struct ValidationField {
    /// The field name (a JS-safe identifier).
    pub(crate) name: String,
    /// Whether the field is optional (skipped when `undefined`).
    pub(crate) optional: bool,
    /// The `typeof` string (`"number"`, `"string"`, `"boolean"`) for primitive
    /// fields, or `None` for non-primitive fields (no typeof check emitted).
    pub(crate) ts_typeof: Option<&'static str>,
    /// The TypeScript boolean expression for a `constraint`, if any.
    pub(crate) constraint: Option<String>,
}

/// Returns the JS `typeof` tag for a primitive resolved [`Type`], or `None`.
pub(crate) fn ts_typeof_of(ty: &Type) -> Option<&'static str> {
    match ty {
        Type::Int | Type::Float => Some("number"),
        Type::String => Some("string"),
        Type::Bool => Some("boolean"),
        _ => None,
    }
}

/// Splits `s` on top-level occurrences of ` {op} ` (e.g. ` && ` or ` || `),
/// ignoring operators nested inside parentheses/brackets/braces or string
/// literals. The operator and its flanking spaces are dropped from the parts.
///
/// String tracking toggles on any `"` and does NOT handle a backslash-escaped
/// quote. That is safe only because the constraint expressions fed here emit
/// string literals unescaped (see `constraint_expr_prec`) and those literals
/// never contain a break operator. If escaped string literals are ever emitted,
/// this scanner must learn to skip `\"` in lockstep with that change.
pub(crate) fn split_top_level<'a>(s: &'a str, op: &str) -> Vec<&'a str> {
    let needle = format!(" {op} ");
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut start = 0usize;
    // Iterate by `char` so every index is a valid UTF-8 boundary. Phoenix allows
    // non-ASCII identifiers, so the expression can contain multi-byte chars; raw
    // byte indexing into `s[i..]` would panic mid-character. (The needle is all
    // ASCII, so a match can only start on a single-byte space anyway, but the
    // boundary-safe indexing is what makes the surrounding slices sound.)
    let mut iter = s.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if in_str {
            if c == '"' {
                in_str = false;
            }
        } else {
            match c {
                '"' => in_str = true,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                _ => {}
            }
            if depth == 0 && s[i..].starts_with(&needle) {
                parts.push(&s[start..i]);
                start = i + needle.len();
                // Skip the chars the needle consumed so the next iteration
                // resumes after it.
                while iter.peek().is_some_and(|&(j, _)| j < start) {
                    iter.next();
                }
            }
        }
    }
    parts.push(&s[start..]);
    parts
}

/// Binary operators Prettier may break a long expression at, ordered from
/// lowest to highest precedence. Breaking happens at the lowest-precedence
/// top-level operator present, so this order is significant; the multi-character
/// relational/equality forms precede their single-character prefixes so a needle
/// like ` < ` can't be matched inside ` <= ` (the surrounding spaces already
/// prevent that, but the ordering keeps intent clear).
pub(crate) const BREAK_OPERATORS: &[&str] = &[
    "||", "&&", "===", "!==", "<=", ">=", "<", ">", "+", "-", "*", "/", "%",
];

/// Tries to break an overflowing expression at its lowest-precedence top-level
/// binary operator, emitting `lhs op` then each subsequent operand on its own
/// line at the same `indent` (Prettier's binary-expression layout). `sep` is
/// appended after the final operand. Returns `false` if no top-level operator
/// was found (nothing to break).
pub(crate) fn break_binary_expr(out: &mut String, indent: &str, expr: &str, sep: &str) -> bool {
    for op in BREAK_OPERATORS {
        let parts = split_top_level(expr, op);
        if parts.len() > 1 {
            let last = parts.len() - 1;
            for (i, p) in parts.iter().enumerate() {
                if i < last {
                    out.push_str(&format!("{indent}{p} {op}\n"));
                } else {
                    out.push_str(&format!("{indent}{p}{sep}\n"));
                }
            }
            return true;
        }
    }
    false
}

/// Emits one conjunct of a broken `if (...)` condition at `indent`, appending
/// `sep` (`" &&"` for a non-final conjunct, `""` for the last). When the
/// conjunct overflows, it is broken the way Prettier does: a negation `!(...)`
/// has its inner operands split at their lowest-precedence top-level operator
/// (`||` before `&&`) inside a `!(` … `)` block; any other expression breaks
/// directly at its own lowest-precedence top-level operator.
pub(crate) fn emit_condition_part(out: &mut String, indent: &str, part: &str, sep: &str) {
    let line = format!("{indent}{part}{sep}");
    if line.len() <= PRINT_WIDTH {
        out.push_str(&line);
        out.push('\n');
        return;
    }

    if let Some(inner) = part.strip_prefix("!(").and_then(|s| s.strip_suffix(')')) {
        // `||` binds looser than `&&`, so Prettier breaks there first.
        let or_parts = split_top_level(inner, "||");
        let (inner_parts, inner_op) = if or_parts.len() > 1 {
            (or_parts, "||")
        } else {
            (split_top_level(inner, "&&"), "&&")
        };
        if inner_parts.len() > 1 {
            out.push_str(&format!("{indent}!(\n"));
            for (i, p) in inner_parts.iter().enumerate() {
                let isep = if i + 1 < inner_parts.len() {
                    format!(" {inner_op}")
                } else {
                    String::new()
                };
                out.push_str(&format!("{indent}  {p}{isep}\n"));
            }
            out.push_str(&format!("{indent}){sep}\n"));
            return;
        }
    }

    // A plain overflowing expression (e.g. a long `typeof obj.x !== "string"`):
    // break at its top-level binary operator.
    if break_binary_expr(out, indent, part, sep) {
        return;
    }

    // No top-level operator to break at; emit as-is.
    out.push_str(&line);
    out.push('\n');
}

/// Emits a `throw new ValidationError("msg");` statement at `indent`, dropping
/// the message argument onto its own line (Prettier style) when the single-line
/// form exceeds the print width.
pub(crate) fn emit_throw(out: &mut String, indent: &str, msg: &str) {
    let one_line = format!("{indent}throw new ValidationError(\"{msg}\");");
    if one_line.len() <= PRINT_WIDTH {
        out.push_str(&one_line);
        out.push('\n');
    } else {
        out.push_str(&format!(
            "{indent}throw new ValidationError(\n{indent}  \"{msg}\",\n{indent});\n"
        ));
    }
}

/// Emits an `if (cond) throw new ValidationError(msg);` statement, matching
/// Prettier's wrapping:
///   * fits on one line → single line;
///   * `if (...)` line fits but the whole statement doesn't → condition stays
///     on the `if` line, the `throw` drops to the next indented line;
///   * `if (...)` line itself overflows → each top-level `&&` conjunct goes on
///     its own line inside a broken `if (`, with any overflowing `!(...)`
///     conjunct broken further (see [`emit_condition_part`]) and the `throw` on
///     the following indented line.
///
/// The `throw` is itself wrapped when its message makes the line overflow (see
/// [`emit_throw`]), independently of how the condition is laid out.
///
/// `conjuncts` is the list of top-level `&&`-joined condition parts.
pub(crate) fn emit_guard(out: &mut String, indent: &str, conjuncts: &[String], msg: &str) {
    let cond = conjuncts.join(" && ");
    let throw = format!("throw new ValidationError(\"{msg}\");");

    let one_line = format!("{indent}if ({cond}) {throw}");
    if one_line.len() <= PRINT_WIDTH {
        out.push_str(&one_line);
        out.push('\n');
        return;
    }

    let throw_indent = format!("{indent}  ");
    let if_line = format!("{indent}if ({cond})");
    if if_line.len() <= PRINT_WIDTH {
        // Condition fits on the `if` line: keep it there, drop the throw below.
        out.push_str(&format!("{if_line}\n"));
        emit_throw(out, &throw_indent, msg);
        return;
    }

    // Break the condition across lines at top-level `&&`.
    out.push_str(&format!("{indent}if (\n"));
    let part_indent = format!("{indent}  ");
    for (i, part) in conjuncts.iter().enumerate() {
        let sep = if i + 1 < conjuncts.len() { " &&" } else { "" };
        emit_condition_part(out, &part_indent, part, sep);
    }
    out.push_str(&format!("{indent})\n"));
    emit_throw(out, &throw_indent, msg);
}

/// Emits the shared body of a `validate*` function: the object guard, the cast
/// to `Record`, and per-field type/constraint guards.
pub(crate) fn emit_validation_body(out: &mut String, fields: &[ValidationField]) {
    emit_guard(
        out,
        "  ",
        &["typeof input !== \"object\" || input === null".to_string()],
        "expected object",
    );
    out.push_str("  const obj = input as Record<string, unknown>;\n");

    for f in fields {
        let name = &f.name;
        if let Some(ty) = f.ts_typeof {
            let mut conds = Vec::new();
            if f.optional {
                conds.push(format!("obj.{name} !== undefined"));
            }
            conds.push(format!("typeof obj.{name} !== \"{ty}\""));
            emit_guard(out, "  ", &conds, &format!("{name}: expected {ty}"));
        }

        if let Some(ref expr) = f.constraint {
            let mut conds = Vec::new();
            if f.optional {
                conds.push(format!("obj.{name} !== undefined"));
            }
            conds.push(format!("!({expr})"));
            emit_guard(out, "  ", &conds, &format!("{name}: constraint violated"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A required param sets unconditionally (no `!== undefined` guard or `opts?.`
    /// chain — both would trip eslint's `no-unnecessary-condition`).
    #[test]
    fn emit_param_set_required_is_unconditional() {
        let mut out = String::new();
        emit_param_set(&mut out, "    ", "term", false, false);
        assert_eq!(out, "    params.set(\"term\", String(opts.term));\n");
    }

    /// An over-long *required* `params.set(...)` breaks one argument per line with
    /// a magic trailing comma and closes `)` at the param's own indent — the same
    /// Prettier layout the optional branch uses. The generator's real field names
    /// are short, so this is the only coverage of that break.
    #[test]
    fn emit_param_set_required_breaks_when_overflowing() {
        let mut out = String::new();
        let name = "aReallyExtremelyLongRequiredQueryParameterNameThatOverflows";
        emit_param_set(&mut out, "    ", name, false, false);
        assert_eq!(
            out,
            format!("    params.set(\n      \"{name}\",\n      String(opts.{name}),\n    );\n")
        );
    }

    /// An optional param on a *nullable* `opts` (every param optional) guards with
    /// `opts?.`; on a non-nullable `opts` (some param required) a plain `opts.` is
    /// used so the `?.` is not a redundant chain.
    #[test]
    fn emit_param_set_optional_access_tracks_opts_nullability() {
        let mut nullable = String::new();
        emit_param_set(&mut nullable, "    ", "page", true, true);
        assert_eq!(
            nullable,
            "    if (opts?.page !== undefined) params.set(\"page\", String(opts.page));\n"
        );

        let mut non_nullable = String::new();
        emit_param_set(&mut non_nullable, "    ", "page", true, false);
        assert_eq!(
            non_nullable,
            "    if (opts.page !== undefined) params.set(\"page\", String(opts.page));\n"
        );
    }
}
