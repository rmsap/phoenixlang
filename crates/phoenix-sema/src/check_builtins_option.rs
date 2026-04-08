use crate::checker::Checker;
use crate::types::Type;
use phoenix_parser::ast::MethodCallExpr;

impl Checker {
    /// Type-checks built-in `Option<T>` combinator calls.
    pub(crate) fn check_option_method(
        &mut self,
        mc: &MethodCallExpr,
        inner_type: Type,
    ) -> Option<Type> {
        match mc.method.as_str() {
            "map" => {
                if self.expect_arg_count(mc, 1)
                    && let Some((_, ret)) =
                        self.check_closure_arg(mc, 0, 1, Some(&[&inner_type]), None)
                {
                    return Some(Type::Generic("Option".to_string(), vec![ret]));
                }
                Some(Type::Generic(
                    "Option".to_string(),
                    vec![Type::TypeVar("U".to_string())],
                ))
            }
            "andThen" => {
                if self.expect_arg_count(mc, 1)
                    && let Some((_, ret)) =
                        self.check_closure_arg(mc, 0, 1, Some(&[&inner_type]), None)
                {
                    if let Type::Generic(ref name, _) = ret
                        && name == "Option"
                    {
                        return Some(ret);
                    }
                    if !ret.is_error() && !ret.has_type_vars() {
                        self.error(
                            format!("andThen callback must return Option, got {}", ret),
                            mc.args[0].span(),
                        );
                    }
                    return Some(ret);
                }
                Some(Type::Generic(
                    "Option".to_string(),
                    vec![Type::TypeVar("U".to_string())],
                ))
            }
            "orElse" => {
                let expected_ret = Type::Generic("Option".to_string(), vec![inner_type.clone()]);
                if self.expect_arg_count(mc, 1)
                    && let Some((_, ret)) =
                        self.check_closure_arg(mc, 0, 0, None, Some(&expected_ret))
                {
                    return Some(ret);
                }
                Some(Type::Generic("Option".to_string(), vec![inner_type]))
            }
            "filter" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_closure_arg(mc, 0, 1, Some(&[&inner_type]), Some(&Type::Bool));
                }
                Some(Type::Generic("Option".to_string(), vec![inner_type]))
            }
            "unwrapOrElse" => {
                if self.expect_arg_count(mc, 1)
                    && let Some((_, ret)) =
                        self.check_closure_arg(mc, 0, 0, None, Some(&inner_type))
                {
                    return Some(ret);
                }
                Some(inner_type)
            }
            "okOr" => {
                if self.expect_arg_count(mc, 1) {
                    let err_type = self.check_expr(&mc.args[0]);
                    return Some(Type::Generic(
                        "Result".to_string(),
                        vec![inner_type, err_type],
                    ));
                }
                Some(Type::Generic(
                    "Result".to_string(),
                    vec![inner_type, Type::TypeVar("E".to_string())],
                ))
            }
            _ => None,
        }
    }
}
