use crate::checker::Checker;
use crate::types::Type;
use phoenix_parser::ast::MethodCallExpr;

impl Checker {
    /// Type-checks built-in `Map<K, V>` method calls.
    pub(crate) fn check_map_method(
        &mut self,
        mc: &MethodCallExpr,
        key_type: Type,
        val_type: Type,
    ) -> Option<Type> {
        match mc.method.as_str() {
            "length" => {
                self.expect_arg_count(mc, 0);
                Some(Type::Int)
            }
            "get" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_method_arg(mc, 0, &key_type);
                }
                Some(Type::Generic("Option".to_string(), vec![val_type]))
            }
            "contains" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_method_arg(mc, 0, &key_type);
                }
                Some(Type::Bool)
            }
            "set" => {
                if self.expect_arg_count(mc, 2) {
                    self.check_method_arg(mc, 0, &key_type);
                    self.check_method_arg(mc, 1, &val_type);
                }
                Some(crate::types::map_of(key_type, val_type))
            }
            "remove" => {
                if self.expect_arg_count(mc, 1) {
                    self.check_method_arg(mc, 0, &key_type);
                }
                Some(crate::types::map_of(key_type, val_type))
            }
            "keys" => {
                self.expect_arg_count(mc, 0);
                Some(crate::types::list_of(key_type))
            }
            "values" => {
                self.expect_arg_count(mc, 0);
                Some(crate::types::list_of(val_type))
            }
            _ => {
                self.error(format!("no method `{}` on type `Map`", mc.method), mc.span);
                Some(Type::Error)
            }
        }
    }
}
