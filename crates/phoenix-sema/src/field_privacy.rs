//! Cross-module field-privacy enforcement.
//!
//! Read sites (`obj.field`) emit one diagnostic per access via
//! [`Checker::enforce_cross_module_field_read_privacy`]. Construction
//! sites (`Foo(arg, …)`) write every field at once, so violations are
//! batched into a single diagnostic via
//! [`Checker::enforce_cross_module_construction_privacy`].

use phoenix_common::diagnostics::Diagnostic;
use phoenix_common::span::Span;
use phoenix_parser::ast::Visibility;

use crate::checker::{Checker, FieldInfo, StructInfo};

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
}
