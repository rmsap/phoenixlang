//! Shared AST walker for sema-side predicates and validators that need
//! to scan an `Expr` (or a `Statement`) for nested constructs.
//!
//! The defer placement / `return` / `?` checks all share the same
//! recursion shape over `Expr` and `Statement`. This module factors
//! that recursion into a single [`Walk`] visitor; the per-node decision
//! is the visitor's, the recursion is the walker's.
//!
//! The walker stops at lambda boundaries: a `return` or `?` inside a
//! lambda body targets the lambda, not the enclosing function, so all
//! current callers want lambdas treated as opaque leaves. If a future
//! check needs to recurse into lambdas, that visitor can handle
//! `Expr::Lambda` in `on_expr` and manually walk the body before
//! returning [`WalkAction::Skip`].

use phoenix_parser::ast::{
    ElseBranch, Expr, ForSource, IfExpr, MatchBody, MatchExpr, Statement, StringSegment,
};

/// Per-node decision returned by [`Walk::on_expr`] / [`Walk::on_stmt`].
pub(crate) enum WalkAction {
    /// Don't recurse into this node's children, but keep walking siblings.
    /// Lets a visitor manually recurse with side state set up first —
    /// e.g. bumping a "we're inside a nested defer" depth counter —
    /// without the outer walker also recursing and double-processing
    /// the children.
    Skip,
    /// Recurse into this node's children (default).
    Recurse,
}

/// Visitor for [`walk_expr`] / [`walk_stmt`]. Defaults recurse without
/// matching any node, so an empty impl is a no-op walk.
pub(crate) trait Walk {
    fn on_expr(&mut self, _: &Expr) -> WalkAction {
        WalkAction::Recurse
    }
    fn on_stmt(&mut self, _: &Statement) -> WalkAction {
        WalkAction::Recurse
    }
}

/// Walk `expr` and every reachable sub-expression / embedded statement
/// (in `if`-arms and `match`-arm blocks). Lambda bodies are *not*
/// recursed into — they are leaves. Visitors observe each node via
/// [`Walk::on_expr`] / [`Walk::on_stmt`] and accumulate state on
/// `&mut self`.
pub(crate) fn walk_expr<W: Walk + ?Sized>(w: &mut W, expr: &Expr) {
    match w.on_expr(expr) {
        WalkAction::Skip => return,
        WalkAction::Recurse => {}
    }
    match expr {
        Expr::Literal(_) | Expr::Ident(_) | Expr::Lambda(_) => {}
        Expr::Binary(b) => {
            walk_expr(w, &b.left);
            walk_expr(w, &b.right);
        }
        Expr::Unary(u) => walk_expr(w, &u.operand),
        Expr::Call(c) => {
            walk_expr(w, &c.callee);
            for a in &c.args {
                walk_expr(w, a);
            }
            for (_, e) in &c.named_args {
                walk_expr(w, e);
            }
        }
        Expr::Assignment(a) => walk_expr(w, &a.value),
        Expr::FieldAssignment(fa) => {
            walk_expr(w, &fa.object);
            walk_expr(w, &fa.value);
        }
        Expr::FieldAccess(fa) => walk_expr(w, &fa.object),
        Expr::MethodCall(mc) => {
            walk_expr(w, &mc.object);
            for a in &mc.args {
                walk_expr(w, a);
            }
        }
        Expr::StructLiteral(sl) => {
            for a in &sl.args {
                walk_expr(w, a);
            }
        }
        Expr::Match(m) => walk_match(w, m),
        Expr::If(if_expr) => walk_if(w, if_expr),
        Expr::ListLiteral(ll) => {
            for e in &ll.elements {
                walk_expr(w, e);
            }
        }
        Expr::MapLiteral(ml) => {
            for (k, v) in &ml.entries {
                walk_expr(w, k);
                walk_expr(w, v);
            }
        }
        Expr::StringInterpolation(si) => {
            for seg in &si.segments {
                if let StringSegment::Expr(e) = seg {
                    walk_expr(w, e);
                }
            }
        }
        Expr::Try(t) => walk_expr(w, &t.operand),
    }
}

/// Walk `stmt` and every reachable sub-expression / nested statement.
pub(crate) fn walk_stmt<W: Walk + ?Sized>(w: &mut W, stmt: &Statement) {
    match w.on_stmt(stmt) {
        WalkAction::Skip => return,
        WalkAction::Recurse => {}
    }
    match stmt {
        Statement::Expression(es) => walk_expr(w, &es.expr),
        Statement::VarDecl(vd) => walk_expr(w, &vd.initializer),
        Statement::Return(r) => {
            if let Some(e) = &r.value {
                walk_expr(w, e);
            }
        }
        Statement::While(wh) => {
            walk_expr(w, &wh.condition);
            for s in &wh.body.statements {
                walk_stmt(w, s);
            }
            if let Some(b) = &wh.else_block {
                for s in &b.statements {
                    walk_stmt(w, s);
                }
            }
        }
        Statement::For(f) => {
            match &f.source {
                ForSource::Range { start, end } => {
                    walk_expr(w, start);
                    walk_expr(w, end);
                }
                ForSource::Iterable(e) => walk_expr(w, e),
            }
            for s in &f.body.statements {
                walk_stmt(w, s);
            }
            if let Some(b) = &f.else_block {
                for s in &b.statements {
                    walk_stmt(w, s);
                }
            }
        }
        Statement::Defer(d) => walk_expr(w, &d.expr),
        Statement::Break(_) | Statement::Continue(_) => {}
    }
}

fn walk_if<W: Walk + ?Sized>(w: &mut W, if_expr: &IfExpr) {
    walk_expr(w, &if_expr.condition);
    for s in &if_expr.then_block.statements {
        walk_stmt(w, s);
    }
    match &if_expr.else_branch {
        Some(ElseBranch::Block(b)) => {
            for s in &b.statements {
                walk_stmt(w, s);
            }
        }
        Some(ElseBranch::ElseIf(nested)) => walk_if(w, nested),
        None => {}
    }
}

fn walk_match<W: Walk + ?Sized>(w: &mut W, m: &MatchExpr) {
    walk_expr(w, &m.subject);
    for arm in &m.arms {
        match &arm.body {
            MatchBody::Expr(e) => walk_expr(w, e),
            MatchBody::Block(b) => {
                for s in &b.statements {
                    walk_stmt(w, s);
                }
            }
        }
    }
}
