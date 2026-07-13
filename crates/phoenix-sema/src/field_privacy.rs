//! Cross-module field-privacy enforcement.
//!
//! Read sites (`obj.field`) emit one diagnostic per access via
//! [`Checker::enforce_cross_module_field_read_privacy`]. Construction
//! sites (`Foo(arg, …)`) write every field at once, so violations are
//! batched into a single diagnostic via
//! [`Checker::enforce_cross_module_construction_privacy`].
//! JSON serialization reads (`json.encode`) or constructs (`json.decode`)
//! every reachable struct, so both walk the target type through
//! [`Checker::json_field_privacy_violation`].

use phoenix_common::diagnostics::Diagnostic;
use phoenix_common::span::Span;
use phoenix_parser::ast::Visibility;

use crate::checker::{Checker, FieldInfo, StructInfo};
use crate::types::Type;

impl Checker {
    /// Cross-module field-privacy gate for *read* sites (`obj.field`).
    /// Same-module access and builtin structs short-circuit; a private
    /// field on a foreign struct emits a rich diagnostic.
    ///
    /// Construction (`Foo(arg, …)`) goes through
    /// [`Self::enforce_cross_module_construction_privacy`] instead, so
    /// that *all* offending private fields surface in a single batched
    /// diagnostic rather than N separate ones anchored at the same span.
    pub(crate) fn enforce_cross_module_field_read_privacy(
        &mut self,
        struct_info: &StructInfo,
        type_name: &str,
        field: &FieldInfo,
        use_span: Span,
    ) {
        if struct_info.def_module == self.current_module || struct_info.def_module.is_builtin() {
            return;
        }
        if field.visibility != Visibility::Private {
            return;
        }
        self.emit_private_access_diagnostic(
            format!(
                "field `{}` of struct `{}` is private to module `{}`",
                field.name, type_name, struct_info.def_module
            ),
            use_span,
            field.definition_span,
            format!(
                "mark `{}` as `public` in the struct definition to export it",
                field.name
            ),
        );
    }

    /// Cross-module field-privacy gate for *write* sites
    /// (`obj.field = value`). Same-module access and builtin structs
    /// short-circuit; a private field on a foreign struct emits a rich
    /// diagnostic that frames the violation as a write ("cannot be set
    /// from outside that module"), matching the construction-side
    /// wording.
    ///
    /// Sibling of [`Self::enforce_cross_module_field_read_privacy`]
    /// (read sites) and [`Self::enforce_cross_module_construction_privacy`]
    /// (positional construction).
    pub(crate) fn enforce_cross_module_field_write_privacy(
        &mut self,
        struct_info: &StructInfo,
        type_name: &str,
        field: &FieldInfo,
        use_span: Span,
    ) {
        if struct_info.def_module == self.current_module || struct_info.def_module.is_builtin() {
            return;
        }
        if field.visibility != Visibility::Private {
            return;
        }
        self.emit_private_access_diagnostic(
            format!(
                "field `{}` of struct `{}` is private to module `{}` \
                 and cannot be set from outside that module",
                field.name, type_name, struct_info.def_module
            ),
            use_span,
            field.definition_span,
            format!(
                "mark `{}` as `public` in the struct definition to allow assignment",
                field.name
            ),
        );
    }

    /// Cross-module construction-privacy gate. Positional construction
    /// (`Foo(arg, …)`) writes every field, so any private field on a
    /// foreign struct is a violation. Emits a single batched diagnostic
    /// listing every offending field — N separate diagnostics on the
    /// same construction span would be noisy.
    ///
    /// Same-module access and builtin structs short-circuit. The
    /// emitted diagnostic carries one note per offending field
    /// (pointing at its declaration) and a single suggestion to mark
    /// the offending fields `public`.
    pub(crate) fn enforce_cross_module_construction_privacy(
        &mut self,
        struct_info: &StructInfo,
        type_name: &str,
        use_span: Span,
    ) {
        if struct_info.def_module == self.current_module || struct_info.def_module.is_builtin() {
            return;
        }
        let private_fields: Vec<&FieldInfo> = struct_info
            .fields
            .iter()
            .filter(|f| f.visibility == Visibility::Private)
            .collect();
        if private_fields.is_empty() {
            return;
        }

        let field_list = private_fields
            .iter()
            .map(|f| format!("`{}`", f.name))
            .collect::<Vec<_>>()
            .join(", ");
        let (noun, verb) = if private_fields.len() == 1 {
            ("field", "is")
        } else {
            ("fields", "are")
        };
        let mut diag = Diagnostic::error(
            format!(
                "{noun} {field_list} of struct `{type_name}` {verb} private to module `{}` \
                 and cannot be set from outside that module",
                struct_info.def_module
            ),
            use_span,
        );
        for f in &private_fields {
            diag = diag.with_note(f.definition_span, "declared here");
        }
        diag = diag.with_suggestion(format!(
            "mark {field_list} as `public` in the struct definition to allow construction"
        ));
        self.diagnostics.push(diag);
    }

    /// Cross-module field-privacy gate for JSON serialization. A synthesized
    /// encoder *reads* — and a synthesized decoder *constructs*, writing —
    /// every field of every struct reachable from the target type, so either
    /// op on a foreign struct with private fields would bypass the read /
    /// write / construction gates above (see design-decisions.md §Phase 4.6
    /// JSON serialization). Walks the type transitively (struct fields, enum
    /// variant payloads, generic arguments) and reports the first offending
    /// struct as one batched diagnostic, mirroring the construction gate's
    /// shape. Returns `true` when a violation was reported so the caller can
    /// skip recording the call site for synthesis.
    ///
    /// `op` is the user-facing operation name (`"json.encode"` /
    /// `"json.decode"`); it selects the verb in the diagnostic.
    pub(crate) fn json_field_privacy_violation(
        &mut self,
        op: &str,
        ty: &Type,
        use_span: Span,
    ) -> bool {
        self.json_privacy_walk(op, ty, use_span, &mut Vec::new())
    }

    /// Recursive body of [`Self::json_field_privacy_violation`]. `visiting`
    /// holds the canonical names of structs/enums currently being walked so
    /// a cyclic type graph terminates (same guard as the JSON support
    /// gates in `check_expr_call.rs`).
    fn json_privacy_walk(
        &mut self,
        op: &str,
        ty: &Type,
        use_span: Span,
        visiting: &mut Vec<String>,
    ) -> bool {
        match ty {
            // `Option<T>` / `List<T>` / `Map<K, V>` (and any future
            // serializable generic): the privacy question lives in the
            // arguments. Generic *user* types are rejected by the support
            // gates before privacy is consulted.
            Type::Generic(_, args) => {
                for arg in args {
                    if self.json_privacy_walk(op, arg, use_span, visiting) {
                        return true;
                    }
                }
                false
            }
            Type::Named(name) => {
                if visiting.iter().any(|n| n == name) {
                    return false; // already being walked (type cycle)
                }
                visiting.push(name.clone());
                let violated = self.json_privacy_check_named(op, name, use_span, visiting);
                visiting.pop();
                violated
            }
            _ => false,
        }
    }

    /// [`Self::json_privacy_walk`]'s `Type::Named` arm: check the struct
    /// itself, then recurse into its fields (or an enum's variant payloads).
    fn json_privacy_check_named(
        &mut self,
        op: &str,
        name: &str,
        use_span: Span,
        visiting: &mut Vec<String>,
    ) -> bool {
        if let Some(info) = self.lookup_struct(name) {
            // Snapshot only what each outcome needs (the walk visits every
            // reachable type, so cloning the whole `StructInfo` per visit
            // would be wasteful): the private-field list for the diagnostic,
            // or the field types for the recursion.
            let foreign = info.def_module != self.current_module && !info.def_module.is_builtin();
            let private_fields: Vec<(String, Span)> = if foreign {
                info.fields
                    .iter()
                    .filter(|f| f.visibility == Visibility::Private)
                    .map(|f| (f.name.clone(), f.definition_span))
                    .collect()
            } else {
                Vec::new()
            };
            if !private_fields.is_empty() {
                let def_module = info.def_module.clone();
                self.emit_json_privacy_violation(op, &def_module, &private_fields, name, use_span);
                return true;
            }
            let field_tys: Vec<Type> = info.fields.iter().map(|f| f.ty.clone()).collect();
            field_tys
                .iter()
                .any(|t| self.json_privacy_walk(op, t, use_span, visiting))
        } else if let Some(info) = self.lookup_enum(name) {
            // Enums have no per-variant privacy; only their payload types
            // can reach a private-field struct.
            let payload_tys: Vec<Type> = info
                .variants
                .iter()
                .flat_map(|(_, tys)| tys.iter().cloned())
                .collect();
            payload_tys
                .iter()
                .any(|t| self.json_privacy_walk(op, t, use_span, visiting))
        } else {
            false
        }
    }

    /// Emit the batched JSON-privacy diagnostic for a foreign struct with
    /// private fields. `private_fields` is the `(name, declaration span)`
    /// list snapshotted by [`Self::json_privacy_check_named`] (never empty —
    /// the caller only invokes this on a violation).
    fn emit_json_privacy_violation(
        &mut self,
        op: &str,
        def_module: &phoenix_common::module_path::ModulePath,
        private_fields: &[(String, Span)],
        type_name: &str,
        use_span: Span,
    ) {
        let field_list = private_fields
            .iter()
            .map(|(name, _)| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let (noun, verb) = if private_fields.len() == 1 {
            ("field", "is")
        } else {
            ("fields", "are")
        };
        let action = if op == "json.decode" {
            "construct"
        } else {
            "serialize"
        };
        let mut diag = Diagnostic::error(
            format!(
                "`{op}` cannot {action} struct `{type_name}` here: {noun} {field_list} \
                 {verb} private to module `{def_module}`"
            ),
            use_span,
        );
        for (_, span) in private_fields {
            diag = diag.with_note(*span, "declared here");
        }
        diag = diag.with_suggestion(format!(
            "mark {field_list} as `public` in the struct definition, \
             or move this `{op}` call into module `{def_module}`"
        ));
        self.diagnostics.push(diag);
    }
}
