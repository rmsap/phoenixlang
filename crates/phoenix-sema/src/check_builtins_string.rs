use crate::checker::Checker;
use crate::types::Type;
use phoenix_parser::ast::MethodCallExpr;

impl Checker {
    /// Type-checks built-in `String` method calls.
    pub(crate) fn check_string_method(&mut self, mc: &MethodCallExpr) -> Option<Type> {
        match mc.method.as_str() {
            "length" => {
                self.expect_arg_count(mc, 0);
                Some(Type::Int)
            }
            "trim" | "toLowerCase" | "toUpperCase" => {
                self.expect_arg_count(mc, 0);
                Some(Type::String)
            }
            "contains" | "startsWith" | "endsWith" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_method_arg(mc, 0, &Type::String);
                }
                Some(Type::Bool)
            }
            "indexOf" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_method_arg(mc, 0, &Type::String);
                }
                Some(Type::Int)
            }
            "split" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_method_arg(mc, 0, &Type::String);
                }
                Some(crate::types::list_of(Type::String))
            }
            "replace" => {
                if self.expect_arg_count(mc, 2) {
                    self.check_method_arg(mc, 0, &Type::String);
                    self.check_method_arg(mc, 1, &Type::String);
                }
                Some(Type::String)
            }
            "substring" => {
                if self.expect_arg_count(mc, 2) {
                    self.check_method_arg(mc, 0, &Type::Int);
                    self.check_method_arg(mc, 1, &Type::Int);
                }
                Some(Type::String)
            }
            _ => {
                self.error(
                    format!("no method `{}` on type `String`", mc.method),
                    mc.span,
                );
                Some(Type::Error)
            }
        }
    }
}
