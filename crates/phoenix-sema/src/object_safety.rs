//! Object-safety validation for traits.
//!
//! A trait is *object-safe* if it can be used as `dyn Trait`: its methods
//! must be dispatchable through a vtable without knowing the concrete `Self`.
//! Run once at trait-registration time; result cached on
//! [`crate::checker::TraitInfo::object_safety_error`]. A non-object-safe
//! trait is still usable as a generic bound (`<T: Trait>`).
//!
//! Rule: `Self` must not appear (directly or nested) in a method's
//! parameter or return types — including nested inside `Generic` args
//! (`Option<Self>`, `List<Self>`) and `Function` types (`Fn(Self) -> T`).
//!
//! **Scope (Phase 2.2 MVP).** Validation is purely syntactic: the walk
//! inspects the method's declared parameter and return `Type`s. It does
//! *not* recursively expand trait bounds — a method signature like
//! `function f<T: SomeOtherTrait>(x: T)` is accepted even if
//! `SomeOtherTrait` itself mentions `Self`. This is sound today because
//! Phoenix does not yet support calling trait-bounded generic methods
//! through a vtable (blocked at IR lowering —
//! see docs/known-issues.md: "`<T: Trait>` method calls fail in compiled
//! mode"). When that gap closes, this pass must extend its walk to
//! follow bound traits, or the compiler must reject bound methods in
//! dyn contexts until bound-expansion lands.

use crate::checker::TraitMethodInfo;
use crate::types::Type;

/// Returns `None` if `methods` is object-safe, `Some(reason)` otherwise.
/// Reason is suitable for embedding after "trait `X` is not object-safe: ".
pub(crate) fn validate(methods: &[TraitMethodInfo]) -> Option<String> {
    for m in methods {
        if m.params.iter().any(contains_self) {
            return Some(format!(
                "method `{}` takes a parameter of type `Self`",
                m.name
            ));
        }
        if contains_self(&m.return_type) {
            return Some(format!("method `{}` returns `Self`", m.name));
        }
    }
    None
}

/// Recursively tests whether `ty` mentions the `Self` type.  Recurses into
/// `Generic` args and `Function` param/return types; stops at everything
/// else.
fn contains_self(ty: &Type) -> bool {
    match ty {
        Type::Named(name) => name == "Self",
        Type::Generic(_, args) => args.iter().any(contains_self),
        Type::Function(params, ret) => params.iter().any(contains_self) || contains_self(ret),
        Type::Dyn(_)
        | Type::Int
        | Type::Float
        | Type::String
        | Type::Bool
        | Type::Void
        | Type::TypeVar(_)
        | Type::Error => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method(name: &str, params: Vec<Type>, ret: Type) -> TraitMethodInfo {
        TraitMethodInfo {
            name: name.to_string(),
            params,
            return_type: ret,
        }
    }

    #[test]
    fn plain_trait_is_object_safe() {
        let methods = vec![method("draw", vec![], Type::String)];
        assert_eq!(validate(&methods), None);
    }

    #[test]
    fn self_in_return_rejects() {
        let methods = vec![method("clone", vec![], Type::Named("Self".into()))];
        assert!(validate(&methods).unwrap().contains("returns `Self`"));
    }

    #[test]
    fn self_in_param_rejects() {
        let methods = vec![method("eq", vec![Type::Named("Self".into())], Type::Bool)];
        assert!(
            validate(&methods)
                .unwrap()
                .contains("parameter of type `Self`")
        );
    }

    #[test]
    fn self_nested_in_generic_rejects() {
        // `Option<Self>` is just as unsafe as bare `Self`.
        let option_self = Type::Generic("Option".into(), vec![Type::Named("Self".into())]);
        let methods = vec![method("maybe", vec![], option_self)];
        assert!(validate(&methods).unwrap().contains("`Self`"));
    }

    #[test]
    fn self_nested_in_function_rejects() {
        let fn_self = Type::Function(vec![Type::Int], Box::new(Type::Named("Self".into())));
        let methods = vec![method("make", vec![fn_self], Type::Void)];
        assert!(validate(&methods).unwrap().contains("`Self`"));
    }

    /// `Self` nested inside a `Result`'s error position must trip the
    /// recursive `contains_self` walk through `Generic` args.
    #[test]
    fn self_nested_in_result_error_position_rejects() {
        let result_self = Type::Generic(
            "Result".into(),
            vec![Type::String, Type::Named("Self".into())],
        );
        let methods = vec![method("try_op", vec![], result_self)];
        let reason = validate(&methods)
            .expect("`Result<String, Self>` return must reject as not object-safe");
        assert!(
            reason.contains("`Self`"),
            "diagnostic should mention `Self`; got: {reason}"
        );
    }

    /// `List<Map<String, Self>>` — depth-2 nesting through two generic
    /// levels. Pins that `contains_self` recurses through every generic
    /// arg, not just the outermost.
    #[test]
    fn self_nested_two_generic_levels_rejects() {
        let inner_map = Type::Generic("Map".into(), vec![Type::String, Type::Named("Self".into())]);
        let outer_list = Type::Generic("List".into(), vec![inner_map]);
        let methods = vec![method("dig", vec![outer_list], Type::Void)];
        assert!(validate(&methods).unwrap().contains("`Self`"));
    }
}
