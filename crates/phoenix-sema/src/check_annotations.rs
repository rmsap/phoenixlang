//! Validation for `@name` / `@name(args)` annotations.
//!
//! Annotations attach compile-time metadata to declarations and fields. This
//! pass checks them against a fixed, compiler-known registry during the
//! registration pass: a built-in annotation applied to the wrong kind of
//! declaration, or with the wrong arguments, is an **error**; an unrecognized
//! annotation is a **warning** (forward-compatible with user-defined
//! annotation processing once `comptime` lands in Phase 5.5).
//!
//! The annotations themselves live on the AST nodes (`StructDecl::annotations`,
//! `FieldDecl::annotations`, …) and are read from there by later consumers —
//! mirroring how doc comments are handled. This pass only validates; it does
//! not copy annotations into the resolved schema.

use crate::checker::Checker;
use phoenix_parser::ast::{Annotation, AnnotationArg};

/// The kind of declaration an annotation is attached to. Built-in annotations
/// are only meaningful on specific targets (e.g. `@jsonName` on a field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnnotationTarget {
    /// A `struct` declaration.
    Struct,
    /// An `enum` declaration.
    Enum,
    /// A free function declaration.
    Function,
    /// A field inside a struct declaration.
    Field,
}

impl AnnotationTarget {
    /// A human-readable description used in diagnostics ("a struct field").
    fn describe(self) -> &'static str {
        match self {
            AnnotationTarget::Struct => "a struct",
            AnnotationTarget::Enum => "an enum",
            AnnotationTarget::Function => "a function",
            AnnotationTarget::Field => "a struct field",
        }
    }
}

impl Checker {
    /// Validates every annotation attached to a declaration or field of kind
    /// `target`, recording diagnostics for unknown names, wrong targets,
    /// malformed argument lists, and duplicates.
    ///
    /// A repeated annotation name is an error: each annotation may appear at
    /// most once per declaration, so a second `@jsonName` can't silently shadow
    /// the first when later stages read them off the AST. Duplicates are
    /// reported once at the offending span and otherwise skipped, so the
    /// surviving first occurrence is still validated normally.
    pub(crate) fn validate_annotations(
        &mut self,
        annotations: &[Annotation],
        target: AnnotationTarget,
    ) {
        let mut seen: Vec<&str> = Vec::with_capacity(annotations.len());
        for ann in annotations {
            if seen.contains(&ann.name.as_str()) {
                self.error(
                    format!(
                        "duplicate annotation `@{}`; an annotation may appear at most once per declaration",
                        ann.name
                    ),
                    ann.span,
                );
                continue;
            }
            seen.push(&ann.name);
            self.validate_annotation(ann, target);
        }
    }

    /// Validates a single annotation against the built-in registry.
    fn validate_annotation(&mut self, ann: &Annotation, target: AnnotationTarget) {
        match ann.name.as_str() {
            // `@jsonName("wire_key")` — Phase 4.6 custom serialization key.
            "jsonName" => {
                self.check_annotation_target(ann, target, AnnotationTarget::Field);
                self.check_single_string_arg(ann);
            }
            // `@skip` — Phase 4.6 exclude a field from serialization.
            "skip" => {
                self.check_annotation_target(ann, target, AnnotationTarget::Field);
                self.check_no_args(ann);
            }
            // `@jsonSerializable` — Phase 4.6 opt-in/out marker on a struct.
            "jsonSerializable" => {
                self.check_annotation_target(ann, target, AnnotationTarget::Struct);
                self.check_no_args(ann);
            }
            // Unknown annotations are ignored, not rejected — this keeps source
            // forward-compatible with annotations a newer compiler understands.
            _ => {
                self.warn(
                    format!("unknown annotation `@{}`; it will be ignored", ann.name),
                    ann.span,
                );
            }
        }
    }

    /// Emits an error if `actual` is not the `expected` target for `ann`.
    fn check_annotation_target(
        &mut self,
        ann: &Annotation,
        actual: AnnotationTarget,
        expected: AnnotationTarget,
    ) {
        if actual != expected {
            self.error(
                format!(
                    "`@{}` can only be applied to {}, not {}",
                    ann.name,
                    expected.describe(),
                    actual.describe()
                ),
                ann.span,
            );
        }
    }

    /// Emits an error if `ann` carries any arguments.
    fn check_no_args(&mut self, ann: &Annotation) {
        if !ann.args.is_empty() {
            self.error(
                format!("`@{}` does not take any arguments", ann.name),
                ann.span,
            );
        }
    }

    /// Emits an error unless `ann` has exactly one string-literal argument.
    fn check_single_string_arg(&mut self, ann: &Annotation) {
        match ann.args.as_slice() {
            [AnnotationArg::String(_)] => {}
            _ => {
                self.error(
                    format!(
                        "`@{}` expects a single string argument, e.g. `@{}(\"value\")`",
                        ann.name, ann.name
                    ),
                    ann.span,
                );
            }
        }
    }
}
