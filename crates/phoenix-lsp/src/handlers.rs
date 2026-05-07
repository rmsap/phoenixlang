//! Pure handler functions for the LSP backend.
//!
//! These are the side-effect-free implementations behind `completion`,
//! `goto_definition`, `references`, and `rename`. They take a borrowed
//! [`DocumentState`] (and a [`Position`] when applicable) and return
//! the result the LSP method should send back; the request URI flows
//! through `state.source_id_to_url`, so handlers don't need it as a
//! separate parameter. Splitting them out lets the tests drive the
//! full request shape without mocking the LSP `Client`.

use std::collections::HashMap;

use phoenix_sema::checker::{FunctionInfo, SymbolKind};
use tower_lsp::lsp_types::*;

use crate::convert::{
    ResolvedSymbol, find_definition_span_for, format_type, position_to_offset, resolve_symbol_ref,
    span_to_range,
};
use crate::state::DocumentState;

/// Build a function completion item with its signature in the detail
/// field. Shared between the scoped and entry-fallback branches of
/// `completion_items_for`.
fn function_completion_item(label: &str, info: &FunctionInfo) -> CompletionItem {
    let params: Vec<String> = info
        .param_names
        .iter()
        .zip(info.params.iter())
        .map(|(n, t)| format!("{}: {}", n, format_type(t)))
        .collect();
    CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::FUNCTION),
        detail: Some(format!(
            "({}) -> {}",
            params.join(", "),
            format_type(&info.return_type)
        )),
        ..Default::default()
    }
}

/// Build the completion items visible in `state`'s current module.
///
/// "Visible in the current module" means: locally-declared names,
/// imported items, and built-ins. Cross-module names that aren't
/// imported into the current module don't appear in completions —
/// the user can't type them anyway. Visibility is sourced from
/// `Analysis::module.module_scopes`, which sema populates with the
/// `local_name → qualified_global_key` map for every parsed module.
///
/// When `module_scopes` doesn't carry an entry for the current module,
/// the fallback only kicks in for the entry-module case (i.e. the
/// single-file analysis path, where sema populates globals against the
/// implicit entry but doesn't emit a `module_scopes` entry). For any
/// non-entry module a missing scope means we know nothing about visible
/// names — return only keywords/built-ins instead of leaking every
/// globally-declared item from sibling modules.
pub(crate) fn completion_items_for(state: &DocumentState) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    let module = &state.check_result.module;
    let scope = module.module_scopes.get(&state.current_module);

    let typed_item = |label: &str, kind: CompletionItemKind| CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        ..Default::default()
    };
    if let Some(scope) = scope {
        for (local_name, qualified) in scope {
            if module.struct_by_name.contains_key(qualified) {
                items.push(typed_item(local_name, CompletionItemKind::STRUCT));
            } else if module.enum_by_name.contains_key(qualified) {
                items.push(typed_item(local_name, CompletionItemKind::ENUM));
            } else if let Some(&id) = module.function_by_name.get(qualified) {
                items.push(function_completion_item(local_name, module.function(id)));
            }
        }
    } else if state.current_module.is_entry() {
        for name in module.struct_by_name.keys() {
            items.push(typed_item(name, CompletionItemKind::STRUCT));
        }
        for name in module.enum_by_name.keys() {
            items.push(typed_item(name, CompletionItemKind::ENUM));
        }
        for (name, &id) in &module.function_by_name {
            items.push(function_completion_item(name, module.function(id)));
        }
    }

    // Keyword completions are sourced from `phoenix_lexer::KEYWORDS`,
    // the canonical lowercase user-facing keyword set, so a new keyword
    // landing in the lexer can't drift out of sync with editor
    // autocomplete. Pinned by
    // `tests::lsp_keyword_completion_covers_every_lexer_keyword`.
    for kw in phoenix_lexer::KEYWORDS {
        items.push(CompletionItem {
            label: kw.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            ..Default::default()
        });
    }
    items
}

/// Resolve the cursor's symbol-at-position to a definition `Location`.
/// Returns `None` if the cursor isn't on a known symbol, its definition
/// span isn't recorded, or its file's URL can't be reconstructed.
pub(crate) fn goto_definition_at(state: &DocumentState, pos: Position) -> Option<Location> {
    let offset = position_to_offset(&state.source, pos);
    for (span, sym_ref) in &state.check_result.symbol_references {
        if span.source_id == state.source_id
            && span.start <= offset
            && offset < span.end
            && let Some(resolved) =
                resolve_symbol_ref(sym_ref, &state.check_result.module, &state.current_module)
            && let Some(def_span) = find_definition_span_for(&resolved, &state.check_result.module)
            && let Some(target_uri) = state.source_id_to_url.get(&def_span.source_id).cloned()
        {
            return Some(Location {
                uri: target_uri,
                range: span_to_range(&def_span, &state.source_map),
            });
        }
    }
    None
}

/// Find the symbol reference under the cursor, if any. Shared between
/// `references_at` and `rename_at` so both look up the cursor's target
/// the same way.
fn target_reference_at(
    state: &DocumentState,
    pos: Position,
) -> Option<&phoenix_sema::checker::SymbolRef> {
    let offset = position_to_offset(&state.source, pos);
    state
        .check_result
        .symbol_references
        .iter()
        .find(|(span, _)| {
            span.source_id == state.source_id && span.start <= offset && offset < span.end
        })
        .map(|(_, r)| r)
}

/// The cursor's symbol identity, pre-computed once per request so the
/// candidate filter doesn't re-resolve the target on every iteration.
///
/// Variables are special-cased: they aren't in `module_scopes`, so we
/// compare on bare `name` *and* require the candidate to live in the
/// same source as the cursor — sema's variable scopes are per-function
/// but the LSP doesn't have a per-function index, so scoping to the
/// file is the safest pragmatic approximation. (Lifting `VarInfo` into
/// the resolved schema, called out in `docs/phases/phase-3.md`, is what
/// unlocks precise variable rename.)
enum TargetIdentity<'a> {
    Variable { name: &'a str },
    Resolved(ResolvedSymbol),
}

/// Compute the cursor target's identity once. Returns `None` for a
/// non-variable target whose name isn't visible in the current module —
/// in that case nothing else in the project can refer to it either.
fn target_identity<'a>(
    state: &DocumentState,
    target_ref: &'a phoenix_sema::checker::SymbolRef,
) -> Option<TargetIdentity<'a>> {
    if matches!(target_ref.kind, SymbolKind::Variable) {
        return Some(TargetIdentity::Variable {
            name: &target_ref.name,
        });
    }
    resolve_symbol_ref(
        target_ref,
        &state.check_result.module,
        &state.current_module,
    )
    .map(TargetIdentity::Resolved)
}

/// Decide whether a candidate reference (at `cand_span` with kind/name
/// `cand_ref`) refers to the same declaration as the pre-computed
/// `target`. Compares qualified [`ResolvedSymbol`] identities so two
/// same-named declarations in different modules don't get conflated.
fn matches_target(
    state: &DocumentState,
    target: &TargetIdentity<'_>,
    cand_span: &phoenix_common::span::Span,
    cand_ref: &phoenix_sema::checker::SymbolRef,
) -> bool {
    match target {
        TargetIdentity::Variable { name } => {
            cand_span.source_id == state.source_id
                && matches!(cand_ref.kind, SymbolKind::Variable)
                && cand_ref.name == *name
        }
        TargetIdentity::Resolved(target_resolved) => {
            let Some(cand_module) = state.source_id_to_module.get(&cand_span.source_id) else {
                return false;
            };
            let Some(cand_resolved) =
                resolve_symbol_ref(cand_ref, &state.check_result.module, cand_module)
            else {
                return false;
            };
            cand_resolved == *target_resolved
        }
    }
}

/// Collect every cross-file `Location` referencing the symbol at the
/// cursor. `None` when the cursor is not on a known symbol.
pub(crate) fn references_at(state: &DocumentState, pos: Position) -> Option<Vec<Location>> {
    let target_ref = target_reference_at(state, pos)?;
    let target = target_identity(state, target_ref)?;
    let locations: Vec<Location> = state
        .check_result
        .symbol_references
        .iter()
        .filter(|(span, r)| matches_target(state, &target, span, r))
        .filter_map(|(span, _)| {
            Some(Location {
                uri: state.source_id_to_url.get(&span.source_id).cloned()?,
                range: span_to_range(span, &state.source_map),
            })
        })
        .collect();

    if locations.is_empty() {
        None
    } else {
        Some(locations)
    }
}

/// Build a `WorkspaceEdit` renaming every cross-file reference of the
/// symbol at the cursor to `new_name`. `None` if the cursor isn't on a
/// known symbol.
///
/// Variables are deliberately rejected: sema's variable scopes are
/// per-function, but the LSP can only approximate "same scope" by
/// "same file" (see [`matches_target`]). Renaming under that
/// approximation would silently rewrite every same-named local across
/// every function in the file, which is destructive. Until `VarInfo`
/// is lifted into `ResolvedModule` (called out in
/// `docs/phases/phase-3.md`), variable rename returns `None` and the
/// editor degrades gracefully.
pub(crate) fn rename_at(
    state: &DocumentState,
    pos: Position,
    new_name: &str,
) -> Option<WorkspaceEdit> {
    let target_ref = target_reference_at(state, pos)?;
    if matches!(target_ref.kind, SymbolKind::Variable) {
        return None;
    }
    let target = target_identity(state, target_ref)?;
    let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
    for (span, sym_ref) in &state.check_result.symbol_references {
        if !matches_target(state, &target, span, sym_ref) {
            continue;
        }
        let Some(span_uri) = state.source_id_to_url.get(&span.source_id).cloned() else {
            continue;
        };
        changes.entry(span_uri).or_default().push(TextEdit {
            range: span_to_range(span, &state.source_map),
            new_text: new_name.to_string(),
        });
    }

    if changes.is_empty() {
        return None;
    }

    Some(WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    })
}

/// Build a hover response for the symbol at `pos`. `None` if no
/// expression is recorded under the cursor.
pub(crate) fn hover_at(state: &DocumentState, pos: Position) -> Option<Hover> {
    let offset = position_to_offset(&state.source, pos);
    for (span, ty) in &state.check_result.module.expr_types {
        if span.source_id == state.source_id && span.start <= offset && offset < span.end {
            let type_str = format_type(ty);
            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!("```phoenix\n{}\n```", type_str),
                }),
                range: Some(span_to_range(span, &state.source_map)),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests;
