//! Free-variable analysis for lambda expressions.
//!
//! Walks a lambda body's AST and collects all variable names that are
//! referenced but not defined locally (by `let`, `for`, match bindings, or
//! the lambda's own parameters).  Used by both the semantic checker
//! (for capture annotation) and the interpreter (for runtime capture).

use crate::ast::{
    Block, ElseBranch, Expr, IfExpr, MatchBody, Pattern, Statement, StringSegment, VarDeclTarget,
};
use std::collections::HashSet;

/// Collects free variable names in `body` that are not in `params` or
/// locally defined within `body`.
pub fn collect_free_variables(body: &Block, params: &[String]) -> HashSet<String> {
    let mut free = HashSet::new();
    let mut local = HashSet::<String>::from_iter(params.iter().cloned());
    walk_block(body, &mut free, &mut local);
    free
}

fn walk_block(block: &Block, free: &mut HashSet<String>, local: &mut HashSet<String>) {
    let snapshot: HashSet<String> = local.clone();
    for stmt in &block.statements {
        walk_stmt(stmt, free, local);
    }
    *local = snapshot;
}

fn walk_stmt(stmt: &Statement, free: &mut HashSet<String>, local: &mut HashSet<String>) {
    match stmt {
        Statement::VarDecl(decl) => {
            walk_expr(&decl.initializer, free, local);
            match &decl.target {
                VarDeclTarget::Simple(name) => {
                    local.insert(name.clone());
                }
                VarDeclTarget::StructDestructure { field_names, .. } => {
                    for name in field_names {
                        local.insert(name.clone());
                    }
                }
            }
        }
        Statement::Expression(es) => walk_expr(&es.expr, free, local),
        Statement::Return(ret) => {
            if let Some(val) = &ret.value {
                walk_expr(val, free, local);
            }
        }
        Statement::While(w) => {
            walk_expr(&w.condition, free, local);
            walk_block(&w.body, free, local);
            if let Some(ref else_block) = w.else_block {
                walk_block(else_block, free, local);
            }
        }
        Statement::For(f) => {
            match &f.source {
                crate::ast::ForSource::Range { start, end } => {
                    walk_expr(start, free, local);
                    walk_expr(end, free, local);
                }
                crate::ast::ForSource::Iterable(iter_expr) => {
                    walk_expr(iter_expr, free, local);
                }
            }
            let snap = local.clone();
            local.insert(f.var_name.clone());
            walk_block(&f.body, free, local);
            *local = snap;
            if let Some(ref else_block) = f.else_block {
                walk_block(else_block, free, local);
            }
        }
        Statement::Break(_) | Statement::Continue(_) => {}
        Statement::Defer(d) => walk_expr(&d.expr, free, local),
    }
}

fn walk_if(if_expr: &IfExpr, free: &mut HashSet<String>, local: &mut HashSet<String>) {
    walk_expr(&if_expr.condition, free, local);
    walk_block(&if_expr.then_block, free, local);
    if let Some(branch) = &if_expr.else_branch {
        match branch {
            ElseBranch::Block(b) => walk_block(b, free, local),
            ElseBranch::ElseIf(nested) => walk_if(nested, free, local),
        }
    }
}

fn walk_expr(expr: &Expr, free: &mut HashSet<String>, local: &mut HashSet<String>) {
    match expr {
        Expr::Ident(ident) => {
            if !local.contains(&ident.name) {
                free.insert(ident.name.clone());
            }
        }
        Expr::Assignment(a) => {
            if !local.contains(&a.name) {
                free.insert(a.name.clone());
            }
            walk_expr(&a.value, free, local);
        }
        Expr::Literal(_) => {}
        Expr::Binary(b) => {
            walk_expr(&b.left, free, local);
            walk_expr(&b.right, free, local);
        }
        Expr::Unary(u) => walk_expr(&u.operand, free, local),
        Expr::Call(c) => {
            walk_expr(&c.callee, free, local);
            for arg in &c.args {
                walk_expr(arg, free, local);
            }
            for (_, expr) in &c.named_args {
                walk_expr(expr, free, local);
            }
        }
        Expr::FieldAssignment(fa) => {
            walk_expr(&fa.object, free, local);
            walk_expr(&fa.value, free, local);
        }
        Expr::FieldAccess(fa) => walk_expr(&fa.object, free, local),
        Expr::MethodCall(mc) => {
            walk_expr(&mc.object, free, local);
            for arg in &mc.args {
                walk_expr(arg, free, local);
            }
        }
        Expr::StructLiteral(sl) => {
            for arg in &sl.args {
                walk_expr(arg, free, local);
            }
        }
        Expr::Match(m) => {
            walk_expr(&m.subject, free, local);
            for arm in &m.arms {
                let snap = local.clone();
                match &arm.pattern {
                    Pattern::Variant(vp) => {
                        for b in &vp.bindings {
                            local.insert(b.clone());
                        }
                    }
                    Pattern::Binding(name, _) => {
                        local.insert(name.clone());
                    }
                    Pattern::Wildcard(_) | Pattern::Literal(_) => {}
                }
                match &arm.body {
                    MatchBody::Expr(e) => walk_expr(e, free, local),
                    MatchBody::Block(b) => walk_block(b, free, local),
                }
                *local = snap;
            }
        }
        Expr::Lambda(inner) => {
            let inner_params: Vec<String> = inner.params.iter().map(|p| p.name.clone()).collect();
            let inner_free = collect_free_variables(&inner.body, &inner_params);
            for name in inner_free {
                if !local.contains(&name) {
                    free.insert(name);
                }
            }
        }
        Expr::ListLiteral(ll) => {
            for elem in &ll.elements {
                walk_expr(elem, free, local);
            }
        }
        Expr::MapLiteral(ml) => {
            for (k, v) in &ml.entries {
                walk_expr(k, free, local);
                walk_expr(v, free, local);
            }
        }
        Expr::StringInterpolation(si) => {
            for seg in &si.segments {
                if let StringSegment::Expr(e) = seg {
                    walk_expr(e, free, local);
                }
            }
        }
        Expr::Try(t) => walk_expr(&t.operand, free, local),
        Expr::If(if_expr) => walk_if(if_expr, free, local),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::*;
    use crate::parser;
    use phoenix_common::span::SourceId;
    use phoenix_lexer::lexer::tokenize;

    fn free_vars_of_lambda(source: &str) -> HashSet<String> {
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);
        find_lambda_free_vars(&program).expect("no lambda found in source")
    }

    fn find_lambda_free_vars(program: &Program) -> Option<HashSet<String>> {
        for decl in &program.declarations {
            if let Declaration::Function(f) = decl {
                for stmt in &f.body.statements {
                    if let Statement::VarDecl(vd) = stmt
                        && let Expr::Lambda(lambda) = &vd.initializer
                    {
                        let params: Vec<String> =
                            lambda.params.iter().map(|p| p.name.clone()).collect();
                        return Some(collect_free_variables(&lambda.body, &params));
                    }
                }
            }
        }
        None
    }

    #[test]
    fn lambda_captures_outer_var() {
        let source = "function main() {\n  let x: Int = 5\n  let f: () -> Int = function() -> Int { return x }\n}";
        let free = free_vars_of_lambda(source);
        assert!(
            free.contains("x"),
            "expected free vars to contain 'x', got {:?}",
            free
        );
    }

    #[test]
    fn lambda_does_not_capture_param() {
        let source =
            "function main() {\n  let f: (Int) -> Int = function(x: Int) -> Int { return x }\n}";
        let free = free_vars_of_lambda(source);
        assert!(
            !free.contains("x"),
            "expected 'x' NOT to be free (it is a param), got {:?}",
            free
        );
    }

    #[test]
    fn lambda_captures_via_named_args() {
        let source = "function foo(a: Int) -> Int { return a }\nfunction main() {\n  let x: Int = 10\n  let f: () -> Int = function() -> Int { return foo(a: x) }\n}";
        let free = free_vars_of_lambda(source);
        assert!(
            free.contains("x"),
            "expected free vars to contain 'x', got {:?}",
            free
        );
        assert!(
            free.contains("foo"),
            "expected free vars to contain 'foo', got {:?}",
            free
        );
    }

    #[test]
    fn lambda_captures_in_match_body() {
        let source = "function main() {\n  let outer: Int = 1\n  let val: Int = 42\n  let f: () -> Int = function() -> Int {\n    match val {\n      1 -> outer\n      _ -> 0\n    }\n  }\n}";
        let free = free_vars_of_lambda(source);
        assert!(
            free.contains("outer"),
            "expected free vars to contain 'outer', got {:?}",
            free
        );
        assert!(
            free.contains("val"),
            "expected free vars to contain 'val', got {:?}",
            free
        );
    }
}
