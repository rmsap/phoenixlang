//! Pure conversion helpers between Phoenix data types and LSP types.
//!
//! All functions in this module are side-effect-free and don't depend
//! on the LSP `Client`; they're trivially testable in isolation.

use std::collections::HashMap;

use phoenix_common::module_path::ModulePath;
use phoenix_common::source::SourceMap;
use phoenix_common::span::{SourceId, Span};
use phoenix_sema::ResolvedModule;
use phoenix_sema::checker::{SymbolKind, SymbolRef};
use phoenix_sema::types::Type;
use tower_lsp::lsp_types::*;

/// Converts a Phoenix `Diagnostic` to an LSP `Diagnostic`.
///
/// Hint and suggestion are appended to the LSP `message` (LSP has no
/// dedicated fields for them) so they show up in IDE tooltips. Notes
/// become `related_information` entries — IDEs render these as
/// clickable cross-references.
///
/// `source_id_to_url` resolves each note's span back to the right URI
/// so cross-file notes (e.g. a privacy diagnostic's "declared here"
/// pointer at another module) link to their actual source. Notes whose
/// `source_id` isn't in the map fall back to `fallback_uri`, which is
/// typically the URI the diagnostic is being published against.
pub(crate) fn to_lsp_diagnostic(
    diag: &phoenix_common::diagnostics::Diagnostic,
    source_map: &SourceMap,
    source_id_to_url: &HashMap<SourceId, Url>,
    fallback_uri: &Url,
) -> Diagnostic {
    let range = span_to_range(&diag.span, source_map);
    let severity = match diag.severity {
        phoenix_common::diagnostics::Severity::Error => DiagnosticSeverity::ERROR,
        phoenix_common::diagnostics::Severity::Warning => DiagnosticSeverity::WARNING,
    };

    let mut message = diag.message.clone();
    if let Some(hint) = &diag.hint {
        message.push_str("\nhint: ");
        message.push_str(hint);
    }
    if let Some(suggestion) = &diag.suggestion {
        message.push_str("\nsuggestion: ");
        message.push_str(suggestion);
    }

    let related_information = if diag.notes.is_empty() {
        None
    } else {
        Some(
            diag.notes
                .iter()
                .map(|note| {
                    let uri = source_id_to_url
                        .get(&note.span.source_id)
                        .cloned()
                        .unwrap_or_else(|| fallback_uri.clone());
                    DiagnosticRelatedInformation {
                        location: Location {
                            uri,
                            range: span_to_range(&note.span, source_map),
                        },
                        message: note.message.clone(),
                    }
                })
                .collect(),
        )
    };

    Diagnostic {
        range,
        severity: Some(severity),
        source: Some("phoenix".to_string()),
        message,
        related_information,
        ..Default::default()
    }
}

/// Converts a Phoenix `Span` to an LSP `Range`.
///
/// The span carries its own [`SourceId`], so no separate parameter is
/// needed; the function resolves line/column against the file the
/// span belongs to.
pub(crate) fn span_to_range(span: &Span, source_map: &SourceMap) -> Range {
    let start = source_map.line_col(span.source_id, span.start);
    let end = source_map.line_col(span.source_id, span.end);
    Range {
        start: Position {
            line: start.line.saturating_sub(1) as u32,
            character: start.col.saturating_sub(1) as u32,
        },
        end: Position {
            line: end.line.saturating_sub(1) as u32,
            character: end.col.saturating_sub(1) as u32,
        },
    }
}

/// Converts an LSP `Position` (0-based line/col) to a byte offset in the source.
///
/// Handles both `\n` and `\r\n` line endings by scanning byte-by-byte rather
/// than relying on `str::lines()` (which strips `\r`).
pub(crate) fn position_to_offset(source: &str, pos: Position) -> usize {
    let target_line = pos.line as usize;
    let target_col = pos.character as usize;
    let mut line = 0;
    let mut col = 0;
    for (i, b) in source.bytes().enumerate() {
        if line == target_line && col == target_col {
            return i;
        }
        if b == b'\n' {
            if line == target_line {
                // Cursor is past end of this line; clamp to end
                return i;
            }
            line += 1;
            col = 0;
        } else if b == b'\r' {
            // Skip \r — the following \n (if any) will advance the line
        } else {
            col += 1;
        }
    }
    // If we get here, cursor is at or past the end of the source
    source.len()
}

/// Formats a Phoenix `Type` as a readable string for hover display.
pub(crate) fn format_type(ty: &Type) -> String {
    match ty {
        Type::Int => "Int".to_string(),
        Type::Float => "Float".to_string(),
        Type::String => "String".to_string(),
        Type::Bool => "Bool".to_string(),
        Type::Void => "Void".to_string(),
        Type::Named(name) => name.clone(),
        Type::Generic(name, args) => {
            let args_str: Vec<String> = args.iter().map(format_type).collect();
            format!("{}<{}>", name, args_str.join(", "))
        }
        Type::Function(params, ret) => {
            let params_str: Vec<String> = params.iter().map(format_type).collect();
            format!("({}) -> {}", params_str.join(", "), format_type(ret))
        }
        Type::TypeVar(name) => name.clone(),
        Type::Dyn(name) => format!("dyn {}", name),
        Type::Error => "?".to_string(),
    }
}

/// A symbol identity that's stable across modules — two `SymbolRef`s
/// resolve to the same `ResolvedSymbol` iff they refer to the same
/// declaration. The qualified strings are the global keys produced by
/// [`ResolvedModule::resolve_visible`]; comparing on those (instead of
/// the bare local names sema records in `symbol_references`) is what
/// keeps cross-file references and rename from conflating same-named
/// declarations in different modules.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ResolvedSymbol {
    Function(String),
    Struct(String),
    Enum(String),
    Field {
        qualified_struct: String,
        name: String,
    },
    Method {
        qualified_type: String,
        name: String,
    },
    EnumVariant {
        qualified_enum: String,
        name: String,
    },
}

/// Resolve a [`SymbolRef`] (which carries only bare local names) into
/// its module-stable [`ResolvedSymbol`] identity, by translating the
/// carrier name(s) through `module_scopes[ref_module]`.
///
/// `ref_module` is the module the reference *lives in*. For the cursor
/// at the call site this is `state.current_module`; for a candidate
/// reference being compared during find-references / rename it's the
/// module owning that reference's `source_id`.
///
/// Returns `None` for `SymbolKind::Variable` (variables aren't tracked
/// in `module_scopes`) and for any name that isn't visible in
/// `ref_module`. The bare-name fallback is deliberately omitted — a
/// non-entry module probing the entry's same-named global would
/// silently collide otherwise.
pub(crate) fn resolve_symbol_ref(
    sym_ref: &SymbolRef,
    cr: &ResolvedModule,
    ref_module: &ModulePath,
) -> Option<ResolvedSymbol> {
    let qualify = |scope_name: &str| -> Option<String> {
        cr.resolve_visible(ref_module, scope_name).map(String::from)
    };
    match &sym_ref.kind {
        SymbolKind::Function => qualify(&sym_ref.name).map(ResolvedSymbol::Function),
        SymbolKind::Struct => qualify(&sym_ref.name).map(ResolvedSymbol::Struct),
        SymbolKind::Enum => qualify(&sym_ref.name).map(ResolvedSymbol::Enum),
        SymbolKind::Variable => None,
        SymbolKind::Field { struct_name } => Some(ResolvedSymbol::Field {
            qualified_struct: qualify(struct_name)?,
            name: sym_ref.name.clone(),
        }),
        SymbolKind::Method { type_name } => Some(ResolvedSymbol::Method {
            qualified_type: qualify(type_name)?,
            name: sym_ref.name.clone(),
        }),
        SymbolKind::EnumVariant { enum_name } => Some(ResolvedSymbol::EnumVariant {
            qualified_enum: qualify(enum_name)?,
            name: sym_ref.name.clone(),
        }),
    }
}

/// Find the definition span for an already-resolved symbol identity.
///
/// Note on `EnumVariant`: today we return the *enum's* definition span,
/// not the variant's own — variant-precise spans aren't tracked on
/// `EnumInfo`. Goto-def on a variant lands you on the enum declaration.
pub(crate) fn find_definition_span_for(
    resolved: &ResolvedSymbol,
    cr: &ResolvedModule,
) -> Option<Span> {
    match resolved {
        ResolvedSymbol::Function(q) => cr.function_info_by_name(q).map(|f| f.definition_span),
        ResolvedSymbol::Struct(q) => cr.struct_info_by_name(q).map(|s| s.definition_span),
        ResolvedSymbol::Enum(q) => cr.enum_info_by_name(q).map(|e| e.definition_span),
        ResolvedSymbol::Field {
            qualified_struct,
            name,
        } => cr.struct_info_by_name(qualified_struct).and_then(|s| {
            s.fields
                .iter()
                .find(|f| f.name == *name)
                .map(|f| f.definition_span)
        }),
        ResolvedSymbol::Method {
            qualified_type,
            name,
        } => cr
            .method_info_by_name(qualified_type, name)
            .map(|m| m.definition_span),
        ResolvedSymbol::EnumVariant { qualified_enum, .. } => cr
            .enum_info_by_name(qualified_enum)
            .map(|e| e.definition_span),
    }
}

#[cfg(test)]
mod tests;
