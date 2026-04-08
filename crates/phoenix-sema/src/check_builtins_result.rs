use crate::checker::Checker;
use crate::types::Type;
use phoenix_parser::ast::MethodCallExpr;

impl Checker {
    /// Type-checks built-in `Result<T, E>` combinator calls.
    pub(crate) fn check_result_method(
        &mut self,
        mc: &MethodCallExpr,
        ok_type: Type,
        err_type: Type,
    ) -> Option<Type> {
        match mc.method.as_str() {
            "map" => {
                if self.expect_arg_count(mc, 1)
                    && let Some((_, ret)) =
                        self.check_closure_arg(mc, 0, 1, Some(&[&ok_type]), None)
                {
                    return Some(Type::Generic("Result".to_string(), vec![ret, err_type]));
                }
                Some(Type::Generic(
                    "Result".to_string(),
                    vec![Type::TypeVar("U".to_string()), err_type],
                ))
            }
            "mapErr" => {
                if self.expect_arg_count(mc, 1)
                    && let Some((_, ret)) =
                        self.check_closure_arg(mc, 0, 1, Some(&[&err_type]), None)
                {
                    return Some(Type::Generic("Result".to_string(), vec![ok_type, ret]));
                }
                Some(Type::Generic(
                    "Result".to_string(),
                    vec![ok_type, Type::TypeVar("F".to_string())],
                ))
            }
            "andThen" => {
                if self.expect_arg_count(mc, 1)
                    && let Some((_, ret)) =
                        self.check_closure_arg(mc, 0, 1, Some(&[&ok_type]), None)
                {
                    if let Type::Generic(ref name, _) = ret
                        && name == "Result"
                    {
                        return Some(ret);
                    }
                    if !ret.is_error() && !ret.has_type_vars() {
                        self.error(
                            format!("andThen callback must return Result, got {}", ret),
                            mc.args[0].span(),
                        );
                    }
                    return Some(ret);
                }
                Some(Type::Generic(
                    "Result".to_string(),
                    vec![Type::TypeVar("U".to_string()), err_type],
                ))
            }
            "orElse" => {
                if self.expect_arg_count(mc, 1)
                    && let Some((_, ret)) =
                        self.check_closure_arg(mc, 0, 1, Some(&[&err_type]), None)
                {
                    if let Type::Generic(ref name, _) = ret
                        && name == "Result"
                    {
                        return Some(ret);
                    }
                    if !ret.is_error() && !ret.has_type_vars() {
                        self.error(
                            format!("orElse callback must return Result, got {}", ret),
                            mc.args[0].span(),
                        );
                    }
                    return Some(ret);
                }
                Some(Type::Generic(
                    "Result".to_string(),
                    vec![ok_type, Type::TypeVar("F".to_string())],
                ))
            }
            "unwrapOrElse" => {
                if self.expect_arg_count(mc, 1)
                    && let Some((_, ret)) =
                        self.check_closure_arg(mc, 0, 1, Some(&[&err_type]), Some(&ok_type))
                {
                    return Some(ret);
                }
                Some(ok_type)
            }
            "ok" => {
                self.expect_arg_count(mc, 0);
                Some(Type::Generic("Option".to_string(), vec![ok_type]))
            }
            "err" => {
                self.expect_arg_count(mc, 0);
                Some(Type::Generic("Option".to_string(), vec![err_type]))
            }
            _ => None,
        }
    }
}
