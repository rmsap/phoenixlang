//! Sema-side checks for `defer` statements.
//!
//! [`Checker::check_defer_placement`] runs once per function/lambda
//! body and enforces three rules in a single AST pass per top-level
//! statement:
//!
//! - **Placement.** Every `Statement::Defer` must sit at the body's
//!   outermost statement level. Anything deeper is rejected.
//! - **No `return` inside a top-level `defer`'s expression.**
//! - **No `?` (try) inside a top-level `defer`'s expression.**
//!
//! `return`/`?` inside a *nested* (already-illegally-placed) defer
//! belong to that nested defer, not to the surrounding top-level one,
//! so the walker tracks a depth counter and only credits hits at
//! depth 0 to the enclosing top-level defer. This avoids piling a
//! misleading "`return` not allowed" error onto an outer defer when
//! the `return` actually lives inside a nested defer's expression.
//!
//! Why reject `return`/`?` inside a defer at all: both lower to
//! `Terminator::Return` on the IR side, and `lower_defers_for_exit`
//! emits every pending defer in sequence into the same block. A
//! mid-defer terminator would leave subsequent defers' lowering
//! writing ops into an already-terminated block (malformed IR) — see
//! the doc-comment on `lower_defers_for_exit` in `phoenix-ir`'s
//! `lower_stmt.rs` for the load-bearing invariant. The AST interp
//! has the parallel runtime concern (`?`'s early-return mid-defer
//! would race with the function's own exit unwrap).

use crate::checker::Checker;
use crate::expr_walk::{Walk, WalkAction, walk_expr, walk_stmt};
use phoenix_common::span::Span;
use phoenix_parser::ast::{Block, Expr, Statement};

/// Single-pass walker that records, for one walk:
/// - spans of every nested `defer` encountered (placement violations)
/// - whether a `return` was seen at depth 0 (i.e. directly inside the
///   top-level deferred expression, not inside a nested defer)
/// - whether a `?` was seen at depth 0
///
/// `nested_depth` is bumped when descending into a `Statement::Defer`'s
/// expression so a nested defer's own `return`/`?` doesn't get
/// attributed to its (already-illegally-placed) outer defer.
struct DeferWalker {
    nested_spans: Vec<Span>,
    saw_return_at_top: bool,
    saw_try_at_top: bool,
    nested_depth: usize,
}

impl DeferWalker {
    fn new() -> Self {
        Self {
            nested_spans: Vec::new(),
            saw_return_at_top: false,
            saw_try_at_top: false,
            nested_depth: 0,
        }
    }
}

impl Walk for DeferWalker {
    fn on_stmt(&mut self, stmt: &Statement) -> WalkAction {
        match stmt {
            Statement::Defer(d) => {
                self.nested_spans.push(d.span);
                // Recurse manually with depth bumped so a deeper-nested
                // defer is still flagged but its `return`/`?` doesn't
                // pollute the outer top-level defer's counts. `Skip`
                // tells the outer walker not to also recurse — we've
                // already handled this node's children.
                self.nested_depth += 1;
                walk_expr(self, &d.expr);
                self.nested_depth -= 1;
                WalkAction::Skip
            }
            Statement::Return(_) if self.nested_depth == 0 => {
                self.saw_return_at_top = true;
                WalkAction::Recurse
            }
            _ => WalkAction::Recurse,
        }
    }

    fn on_expr(&mut self, expr: &Expr) -> WalkAction {
        if self.nested_depth == 0 && matches!(expr, Expr::Try(_)) {
            self.saw_try_at_top = true;
        }
        WalkAction::Recurse
    }
}

const NESTED_PLACEMENT_MSG: &str = "`defer` must appear at the function's outermost statement level, not \
     inside a loop body, conditional, match arm, or other nested block";

impl Checker {
    /// Walks `body` and emits a sema error for any `Statement::Defer`
    /// that is not at the body's outermost statement level, plus
    /// `return`/`?`-inside-defer errors for any top-level defer whose
    /// expression contains those constructs.
    ///
    /// **Why a placement rule.** Both interpreters fire a function's
    /// defers at the function's exit, *after* any inner blocks (loop
    /// body, if-arm, match-arm block) have been popped. A defer that
    /// references a binding from such an inner scope would resolve to
    /// nothing at exit (AST interp: runtime "undefined variable"; IR
    /// lowering: `unreachable!` panic on `lookup_var`). The IR side
    /// also has no active flag, so a defer in an *un*taken branch
    /// would still fire on later exit paths. Restricting `defer` to
    /// the function's outermost block sidesteps both classes of bug.
    /// This is the Phase 2.3 baseline — a future relaxation can lift
    /// the rule once the underlying machinery supports per-iteration
    /// dynamic registration on the IR side.
    ///
    /// Lambda bodies are not recursed into here: each lambda has its
    /// own outermost level and is checked separately when
    /// [`crate::checker::Checker::check_lambda`] runs.
    pub(crate) fn check_defer_placement(&mut self, body: &Block) {
        // Aggregate nested-defer spans across all top-level statements
        // so they can be emitted in one batch at the end (keeps the
        // top-level `return`/`?` errors before the deeper nested
        // placement errors in the diagnostic stream).
        let mut nested_spans: Vec<Span> = Vec::new();

        for stmt in &body.statements {
            match stmt {
                // Top-level defer is allowed. Walk into the deferred
                // expression to find both nested defers (placement
                // errors against them) and any `return`/`?` constructs
                // at depth 0 (errors against THIS defer).
                Statement::Defer(d) => {
                    let mut walker = DeferWalker::new();
                    walk_expr(&mut walker, &d.expr);

                    if walker.saw_return_at_top {
                        self.error(
                            "`return` is not allowed inside a `defer` expression".to_string(),
                            d.span,
                        );
                    }
                    if walker.saw_try_at_top {
                        self.error(
                            "`?` (try) is not allowed inside a `defer` expression".to_string(),
                            d.span,
                        );
                    }
                    nested_spans.extend(walker.nested_spans);
                }
                // Non-defer statements at the top level: any defer
                // reachable from here is by definition nested.
                // `return`/`?` here are legal (it's a top-level
                // statement, not a defer's expression), so the walker's
                // `saw_*_at_top` outputs are ignored.
                other => {
                    let mut walker = DeferWalker::new();
                    walk_stmt(&mut walker, other);
                    nested_spans.extend(walker.nested_spans);
                }
            }
        }

        for span in nested_spans {
            self.error(NESTED_PLACEMENT_MSG.to_string(), span);
        }
    }
}
