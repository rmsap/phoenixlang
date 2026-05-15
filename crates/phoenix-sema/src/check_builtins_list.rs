use crate::checker::Checker;
use crate::types::Type;
use phoenix_parser::ast::MethodCallExpr;

impl Checker {
    /// Type-checks built-in `List<T>` method calls.
    pub(crate) fn check_list_method(
        &mut self,
        mc: &MethodCallExpr,
        elem_type: Type,
    ) -> Option<Type> {
        match mc.method.as_str() {
            "length" => {
                self.expect_arg_count(mc, 0);
                Some(Type::Int)
            }
            "get" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_method_arg(mc, 0, &Type::Int);
                }
                Some(elem_type)
            }
            "push" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_method_arg(mc, 0, &elem_type);
                }
                Some(crate::types::list_of(elem_type))
            }
            "first" | "last" => {
                self.expect_arg_count(mc, 0);
                Some(Type::Generic("Option".to_string(), vec![elem_type]))
            }
            "contains" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_method_arg(mc, 0, &elem_type);
                }
                Some(Type::Bool)
            }
            "take" | "drop" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_method_arg(mc, 0, &Type::Int);
                }
                Some(crate::types::list_of(elem_type))
            }
            "map" => {
                if self.expect_arg_count(mc, 1)
                    && let Some((_, ret)) =
                        self.check_closure_arg(mc, 0, 1, Some(&[&elem_type]), None)
                {
                    return Some(crate::types::list_of(ret));
                }
                Some(crate::types::list_of(Type::TypeVar("U".to_string())))
            }
            "flatMap" => {
                if self.expect_arg_count(mc, 1)
                    && let Some((_, ret)) =
                        self.check_closure_arg(mc, 0, 1, Some(&[&elem_type]), None)
                {
                    if let Type::Generic(name, args) = &ret
                        && name == "List"
                        && args.len() == 1
                    {
                        return Some(crate::types::list_of(args[0].clone()));
                    }
                    if !ret.is_error() && !ret.has_type_vars() {
                        self.error(
                            format!("flatMap callback must return a List, got {}", ret),
                            mc.args[0].span(),
                        );
                        return Some(Type::Error);
                    }
                    return Some(crate::types::list_of(Type::TypeVar("U".to_string())));
                }
                Some(crate::types::list_of(Type::TypeVar("U".to_string())))
            }
            "filter" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_closure_arg(mc, 0, 1, Some(&[&elem_type]), Some(&Type::Bool));
                }
                Some(crate::types::list_of(elem_type))
            }
            "find" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_closure_arg(mc, 0, 1, Some(&[&elem_type]), Some(&Type::Bool));
                }
                Some(Type::Generic("Option".to_string(), vec![elem_type]))
            }
            "any" | "all" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_closure_arg(mc, 0, 1, Some(&[&elem_type]), Some(&Type::Bool));
                }
                Some(Type::Bool)
            }
            "reduce" => {
                if !self.expect_arg_count(mc, 2) {
                    return Some(Type::Error);
                }
                let init_type = self.check_expr(&mc.args[0]);
                if let Some((_, ret)) =
                    self.check_closure_arg(mc, 1, 2, Some(&[&init_type, &elem_type]), None)
                    && !ret.is_error()
                    && !init_type.is_error()
                    && !self.types_compatible(&init_type, &ret)
                {
                    self.error(
                        format!(
                            "reduce callback return type: expected {} but got {}",
                            init_type, ret
                        ),
                        mc.args[1].span(),
                    );
                }
                Some(init_type)
            }
            "sortBy" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_closure_arg(
                        mc,
                        0,
                        2,
                        Some(&[&elem_type, &elem_type]),
                        Some(&Type::Int),
                    );
                }
                Some(crate::types::list_of(elem_type))
            }
            _ => {
                self.error(format!("no method `{}` on type `List`", mc.method), mc.span);
                Some(Type::Error)
            }
        }
    }

    /// Type-checks built-in `ListBuilder<T>` method calls.
    /// The builder is a transient mutable accumulator
    /// constructed via `List.builder()` and consumed via `.freeze()`.
    ///
    /// Use-after-freeze is **runtime-checked** rather than enforced
    /// statically — static enforcement is decision G's deferred
    /// linearity story. The sema-level type after `.freeze()` is
    /// `List<T>`; nothing stops the user from also calling
    /// `b.push(...)` on the consumed builder afterwards, which the
    /// runtime will abort.
    pub(crate) fn check_list_builder_method(
        &mut self,
        mc: &MethodCallExpr,
        elem_type: Type,
    ) -> Option<Type> {
        match mc.method.as_str() {
            "push" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_method_arg(mc, 0, &elem_type);
                }
                // `push` mutates in place and returns nothing — the
                // user-visible API is "method that updates the
                // builder", not "method that returns a new builder".
                // Returning `Void` here is what lets the bench-corpus
                // pattern `for i in 0..n { b.push(i) }` type-check
                // without forcing the user to write
                // `_ = b.push(i)`.
                Some(Type::Void)
            }
            "freeze" => {
                self.expect_arg_count(mc, 0);
                Some(crate::types::list_of(elem_type))
            }
            _ => {
                self.error(
                    format!("no method `{}` on type `ListBuilder`", mc.method),
                    mc.span,
                );
                Some(Type::Error)
            }
        }
    }
}
