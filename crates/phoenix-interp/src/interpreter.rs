use crate::env::Environment;
use crate::value::Value;
use phoenix_common::span::Span;
use phoenix_parser::ast::{
    BinaryExpr, BinaryOp, Block, CallExpr, CaptureInfo, Declaration, ElseBranch, Expr, ForSource,
    ForStmt, FunctionDecl, IfStmt, LiteralKind, MatchBody, MatchExpr, MethodCallExpr, Param,
    Pattern, Program, Statement, StringSegment, StructLiteralExpr, TryExpr, UnaryExpr, UnaryOp,
    WhileStmt,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::rc::Rc;

/// The result of executing a statement, indicating control flow changes.
pub(crate) enum StmtResult {
    /// Execution should proceed to the next statement.
    Continue,
    /// A `return` statement was encountered, carrying the returned value.
    Return(Value),
    /// A `break` statement was encountered, exiting the enclosing loop.
    Break,
    /// A `continue` statement was encountered, skipping to the next iteration.
    LoopContinue,
}

/// An error encountered during program interpretation.
#[derive(Debug)]
pub struct RuntimeError {
    /// A human-readable description of what went wrong at runtime.
    pub message: String,
    /// If set, this is a `?` operator early-return value, not a real error.
    /// The enclosing function call should return this value instead of
    /// propagating the error.
    pub try_return_value: Option<Value>,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}

pub(crate) type Result<T> = std::result::Result<T, RuntimeError>;

pub(crate) fn error<T>(msg: impl Into<String>) -> Result<T> {
    Err(RuntimeError {
        message: msg.into(),
        try_return_value: None,
    })
}

/// Constructs an `Option::Some` variant value.
pub(crate) fn some_val(val: Value) -> Value {
    Value::EnumVariant("Option".to_string(), "Some".to_string(), vec![val])
}

/// Constructs an `Option::None` variant value.
pub(crate) fn none_val() -> Value {
    Value::EnumVariant("Option".to_string(), "None".to_string(), vec![])
}

/// Constructs a `Result::Ok` variant value.
pub(crate) fn ok_val(val: Value) -> Value {
    Value::EnumVariant("Result".to_string(), "Ok".to_string(), vec![val])
}

/// Constructs a `Result::Err` variant value.
pub(crate) fn err_val(val: Value) -> Value {
    Value::EnumVariant("Result".to_string(), "Err".to_string(), vec![val])
}

/// Creates a `RuntimeError` for wrong argument count on a method call.
pub(crate) fn arg_count_error(method: &str, expected: usize, got: usize) -> RuntimeError {
    let noun = if expected == 1 {
        "argument"
    } else {
        "arguments"
    };
    RuntimeError {
        message: format!("{}() takes {} {}, got {}", method, expected, noun, got),
        try_return_value: None,
    }
}

/// Applies a checked integer operation and a floating-point operation to two
/// numeric values. Returns a runtime error for non-numeric or overflow cases.
fn checked_numeric_op(
    left: &Value,
    right: &Value,
    verb: &str,
    int_op: fn(i64, i64) -> Option<i64>,
    float_op: fn(f64, f64) -> f64,
) -> Result<Value> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => {
            Ok(Value::Int(int_op(*a, *b).ok_or_else(|| RuntimeError {
                message: "integer overflow".to_string(),
                try_return_value: None,
            })?))
        }
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(float_op(*a, *b))),
        _ => error(format!(
            "cannot {} {} and {}",
            verb,
            left.type_name(),
            right.type_name()
        )),
    }
}

/// Applies an ordering comparison to two values.
fn compare_values(left: &Value, right: &Value, op: BinaryOp) -> Result<Value> {
    match left.partial_cmp(right) {
        Some(ord) => {
            let result = match op {
                BinaryOp::Lt => ord.is_lt(),
                BinaryOp::Gt => ord.is_gt(),
                BinaryOp::LtEq => ord.is_le(),
                BinaryOp::GtEq => ord.is_ge(),
                _ => unreachable!(),
            };
            Ok(Value::Bool(result))
        }
        None => error(format!(
            "cannot compare {} and {}",
            left.type_name(),
            right.type_name()
        )),
    }
}

/// Struct definition info for the interpreter.
#[derive(Debug, Clone)]
pub(crate) struct StructDef {
    field_names: Vec<String>,
}

/// Enum definition info for the interpreter.
#[derive(Debug, Clone)]
pub(crate) struct EnumDef {
    /// Variant name to field count.
    variants: HashMap<String, usize>,
}

/// Method definition.
#[derive(Debug, Clone)]
pub(crate) struct MethodDef {
    func: FunctionDecl,
}

/// Maximum call depth before the interpreter aborts with a stack overflow error.
const MAX_CALL_DEPTH: usize = 50;

/// The tree-walk interpreter for Phoenix programs.
///
/// Evaluates a Phoenix AST by walking the tree directly, maintaining an
/// environment of variable bindings and registries for functions, structs,
/// enums, and methods.
pub struct Interpreter {
    pub(crate) env: Environment,
    pub(crate) functions: HashMap<String, FunctionDecl>,
    pub(crate) structs: HashMap<String, StructDef>,
    pub(crate) enums: HashMap<String, EnumDef>, // enum name -> def
    pub(crate) variant_to_enum: HashMap<String, String>, // variant name -> enum name
    pub(crate) methods: HashMap<String, HashMap<String, MethodDef>>, // type -> method_name -> def
    pub(crate) call_depth: usize,
    /// Side-channel for propagating break/continue out of expressions (e.g. match blocks).
    pub(crate) pending_control_flow: Option<PendingControlFlow>,
    /// Tracks whether the most recent `StmtResult::Return` was produced by an
    /// explicit `return` statement (`true`) or by an implicit last-expression
    /// return (`false`).  Used in `eval_match` to distinguish function-level
    /// returns from match arm values.
    pub(crate) last_return_was_explicit: bool,
    /// Output sink for `print()`. Defaults to stdout.
    pub(crate) output: Box<dyn Write>,
    /// Captured variables for each lambda, populated by the semantic checker.
    /// Keyed by the lambda's source span.
    pub(crate) lambda_captures: HashMap<Span, Vec<CaptureInfo>>,
}

/// Control flow signal that must escape from an expression context
/// (e.g. a `break` or `continue` inside a match arm block).
#[derive(Debug, Clone, Copy)]
pub(crate) enum PendingControlFlow {
    Break,
    LoopContinue,
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new()
    }
}

impl Interpreter {
    /// Creates a new interpreter with an empty environment and no registered declarations.
    /// Output from `print()` is written to stdout.
    pub fn new() -> Self {
        Self::with_output(Box::new(std::io::stdout()))
    }

    /// Creates a new interpreter that writes `print()` output to the given sink.
    pub fn with_output(output: Box<dyn Write>) -> Self {
        Self {
            env: Environment::new(),
            functions: HashMap::new(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            variant_to_enum: HashMap::new(),
            methods: HashMap::new(),
            call_depth: 0,
            pending_control_flow: None,
            last_return_was_explicit: false,
            lambda_captures: HashMap::new(),
            output,
        }
    }

    /// Registers a slice of function declarations as methods on the given type.
    fn register_methods(&mut self, type_name: &str, methods: &[FunctionDecl]) {
        if methods.is_empty() {
            return;
        }
        let type_methods = self.methods.entry(type_name.to_string()).or_default();
        for func in methods {
            type_methods.insert(func.name.clone(), MethodDef { func: func.clone() });
        }
    }

    /// Unwraps a block execution result into a function return value.
    fn unwrap_call_result(&self, result: Result<StmtResult>) -> Result<Value> {
        match result {
            Ok(StmtResult::Return(value)) => Ok(value),
            Ok(_) => Ok(Value::Void),
            Err(mut e) if e.try_return_value.is_some() => {
                Ok(e.try_return_value.take().expect("guarded by is_some()"))
            }
            Err(e) => Err(e),
        }
    }

    /// Runs a complete Phoenix program by registering all declarations and then
    /// invoking the `main()` function. Returns an error if no `main` is found.
    ///
    /// Before processing user declarations, the built-in `Option<T>` and
    /// `Result<T, E>` enum definitions and variant mappings are pre-registered.
    pub fn run_program(&mut self, program: &Program) -> Result<()> {
        // Register built-in enum variants
        self.variant_to_enum
            .insert("Some".to_string(), "Option".to_string());
        self.variant_to_enum
            .insert("None".to_string(), "Option".to_string());
        self.variant_to_enum
            .insert("Ok".to_string(), "Result".to_string());
        self.variant_to_enum
            .insert("Err".to_string(), "Result".to_string());

        // Register built-in enum definitions
        self.enums.insert(
            "Option".to_string(),
            EnumDef {
                variants: HashMap::from([("Some".to_string(), 1), ("None".to_string(), 0)]),
            },
        );
        self.enums.insert(
            "Result".to_string(),
            EnumDef {
                variants: HashMap::from([("Ok".to_string(), 1), ("Err".to_string(), 1)]),
            },
        );

        // Register all declarations
        for decl in &program.declarations {
            match decl {
                Declaration::Function(func) => {
                    self.functions.insert(func.name.clone(), func.clone());
                }
                Declaration::Struct(s) => {
                    let field_names: Vec<String> =
                        s.fields.iter().map(|f| f.name.clone()).collect();
                    self.structs
                        .insert(s.name.clone(), StructDef { field_names });
                    self.register_methods(&s.name, &s.methods);
                    for ti in &s.trait_impls {
                        self.register_methods(&s.name, &ti.methods);
                    }
                }
                Declaration::Enum(e) => {
                    let mut variants = HashMap::new();
                    for v in &e.variants {
                        variants.insert(v.name.clone(), v.fields.len());
                        self.variant_to_enum.insert(v.name.clone(), e.name.clone());
                    }
                    self.enums.insert(e.name.clone(), EnumDef { variants });
                    self.register_methods(&e.name, &e.methods);
                    for ti in &e.trait_impls {
                        self.register_methods(&e.name, &ti.methods);
                    }
                }
                Declaration::Impl(imp) => {
                    self.register_methods(&imp.type_name, &imp.methods);
                }
                Declaration::Trait(_)
                | Declaration::TypeAlias(_)
                | Declaration::Endpoint(_)
                | Declaration::Schema(_) => {} // Compile-time only
            }
        }

        let main_func = self.functions.get("main").cloned();
        match main_func {
            Some(func) => {
                self.call_function(&func, vec![], vec![])?;
                Ok(())
            }
            None => error("no main() function found"),
        }
    }

    /// Calls a user-defined function with the given arguments, managing scope
    /// and call-depth tracking.  Supports named arguments and default parameter
    /// values.
    fn call_function(
        &mut self,
        func: &FunctionDecl,
        args: Vec<Value>,
        named_args: Vec<(String, Value)>,
    ) -> Result<Value> {
        self.call_depth += 1;
        if self.call_depth > MAX_CALL_DEPTH {
            self.call_depth -= 1;
            return error("stack overflow: maximum recursion depth exceeded");
        }
        self.env.push_scope();

        let non_self_params: Vec<&Param> =
            func.params.iter().filter(|p| p.name != "self").collect();
        let total_params = non_self_params.len();

        // Build a value for each parameter, merging positional, named, and defaults
        let mut param_values: Vec<Option<Value>> = vec![None; total_params];

        // Fill in positional args
        for (i, val) in args.into_iter().enumerate() {
            if i < total_params {
                param_values[i] = Some(val);
            }
        }

        // Fill in named args
        for (name, val) in named_args {
            if let Some(idx) = non_self_params.iter().position(|p| p.name == name) {
                param_values[idx] = Some(val);
            }
        }

        // Fill in defaults for any remaining None slots
        for (i, param) in non_self_params.iter().enumerate() {
            if param_values[i].is_none()
                && let Some(ref default_expr) = param.default_value
            {
                param_values[i] = Some(self.eval_expr(default_expr)?);
            }
        }

        // Check that all params are covered
        for (i, param) in non_self_params.iter().enumerate() {
            match param_values[i].take() {
                Some(val) => self.env.define(param.name.clone(), val),
                None => {
                    self.env.pop_scope();
                    self.call_depth -= 1;
                    return error(format!(
                        "function `{}`: missing argument for parameter `{}`",
                        func.name, param.name
                    ));
                }
            }
        }

        let result = self.exec_block_implicit(&func.body);
        self.env.pop_scope();
        self.call_depth -= 1;

        self.unwrap_call_result(result)
    }

    /// Calls a method on a value by looking up the method in the method registry
    /// and dispatching to `call_function` with `self` bound.
    fn call_method(
        &mut self,
        type_name: &str,
        method_name: &str,
        self_val: Value,
        args: Vec<Value>,
    ) -> Result<Value> {
        self.call_depth += 1;
        if self.call_depth > MAX_CALL_DEPTH {
            self.call_depth -= 1;
            return error("stack overflow: maximum recursion depth exceeded");
        }

        let method = self
            .methods
            .get(type_name)
            .and_then(|m| m.get(method_name))
            .cloned()
            .ok_or_else(|| RuntimeError {
                message: format!("no method `{}` on type `{}`", method_name, type_name),
                try_return_value: None,
            })?;

        self.env.push_scope();
        self.env.define("self".to_string(), self_val);

        let expected = method
            .func
            .params
            .iter()
            .filter(|p| p.name != "self")
            .count();
        if args.len() != expected {
            self.env.pop_scope();
            self.call_depth -= 1;
            return error(format!(
                "method `{}` on `{}` takes {} argument(s), got {}",
                method_name,
                type_name,
                expected,
                args.len()
            ));
        }

        let mut arg_idx = 0;
        for param in &method.func.params {
            if param.name == "self" {
                continue;
            }
            self.env.define(param.name.clone(), args[arg_idx].clone());
            arg_idx += 1;
        }

        let result = self.exec_block_implicit(&method.func.body);
        self.env.pop_scope();
        self.call_depth -= 1;

        self.unwrap_call_result(result)
    }

    /// Executes a block without implicit return.
    ///
    /// Delegates to [`exec_block_inner`] with `implicit_return` set to `false`,
    /// meaning a trailing bare expression is evaluated for side effects only
    /// and does not produce a return value.
    fn exec_block(&mut self, block: &Block) -> Result<StmtResult> {
        self.exec_block_inner(block, false)
    }

    /// Executes a block with implicit return enabled (for function/closure bodies).
    ///
    /// When `implicit_return` is `true` and the last statement is a bare
    /// expression, its value is returned instead of being discarded.
    pub(crate) fn exec_block_implicit(&mut self, block: &Block) -> Result<StmtResult> {
        self.exec_block_inner(block, true)
    }

    /// Executes a block of statements, optionally treating the last expression
    /// as an implicit return value.
    ///
    /// When `implicit_return` is `true` and the final statement is a bare
    /// expression that produces a non-Void value, that value is wrapped in
    /// [`StmtResult::Return`]. This powers Phoenix's implicit-return semantics
    /// for function and closure bodies.
    fn exec_block_inner(&mut self, block: &Block, implicit_return: bool) -> Result<StmtResult> {
        let last_idx = block.statements.len().saturating_sub(1);
        for (i, stmt) in block.statements.iter().enumerate() {
            let is_last = implicit_return && i == last_idx && !block.statements.is_empty();
            // If this is the last statement and it's a bare expression,
            // evaluate it and return its value (implicit return).
            if is_last {
                if let Statement::Expression(expr_stmt) = stmt {
                    let value = self.eval_expr(&expr_stmt.expr)?;
                    if !matches!(value, Value::Void) {
                        self.last_return_was_explicit = false;
                        return Ok(StmtResult::Return(value));
                    }
                    return Ok(StmtResult::Continue);
                }
                // If the last statement is an if/else, propagate implicit
                // return into each branch so the branch value is returned.
                if let Statement::If(if_stmt) = stmt {
                    return self.exec_if_inner(if_stmt, true);
                }
            }
            let result = self.exec_statement(stmt)?;
            // Check for pending control flow from expressions (e.g. break inside match)
            if let Some(cf) = self.pending_control_flow.take() {
                return Ok(match cf {
                    PendingControlFlow::Break => StmtResult::Break,
                    PendingControlFlow::LoopContinue => StmtResult::LoopContinue,
                });
            }
            match &result {
                StmtResult::Continue => {}
                StmtResult::Return(_) | StmtResult::Break | StmtResult::LoopContinue => {
                    return Ok(result);
                }
            }
        }
        Ok(StmtResult::Continue)
    }

    /// Executes a single statement and returns the resulting control flow signal.
    fn exec_statement(&mut self, stmt: &Statement) -> Result<StmtResult> {
        match stmt {
            Statement::VarDecl(var) => {
                let value = self.eval_expr(&var.initializer)?;
                match &var.target {
                    phoenix_parser::ast::VarDeclTarget::Simple(name) => {
                        self.env.define(name.clone(), value);
                    }
                    phoenix_parser::ast::VarDeclTarget::StructDestructure {
                        type_name: _,
                        field_names,
                    } => {
                        if let Value::Struct(_, ref fields) = value {
                            for field_name in field_names {
                                if let Some(field_value) = fields.get(field_name) {
                                    self.env.define(field_name.clone(), field_value.clone());
                                } else {
                                    return error(format!(
                                        "destructuring: no field `{}` in struct",
                                        field_name
                                    ));
                                }
                            }
                        } else {
                            return error("destructuring requires a struct value");
                        }
                    }
                }
                Ok(StmtResult::Continue)
            }
            Statement::Expression(expr_stmt) => {
                self.eval_expr(&expr_stmt.expr)?;
                Ok(StmtResult::Continue)
            }
            Statement::Return(ret) => {
                let value = match &ret.value {
                    Some(expr) => self.eval_expr(expr)?,
                    None => Value::Void,
                };
                self.last_return_was_explicit = true;
                Ok(StmtResult::Return(value))
            }
            Statement::If(if_stmt) => self.exec_if_inner(if_stmt, false),
            Statement::While(w) => self.exec_while(w),
            Statement::For(f) => self.exec_for(f),
            Statement::Break(_) => Ok(StmtResult::Break),
            Statement::Continue(_) => Ok(StmtResult::LoopContinue),
        }
    }

    /// Executes an `if`/`else if`/`else` chain. When `implicit_return` is true,
    /// the last expression in each branch becomes the return value.
    /// Evaluates a slice of expressions into a `Vec<Value>`.
    fn eval_args(&mut self, args: &[Expr]) -> Result<Vec<Value>> {
        args.iter().map(|a| self.eval_expr(a)).collect()
    }

    /// Checks that a method call has the expected argument count, evaluates the
    /// arguments, and returns them. Returns an error on count mismatch.
    pub(crate) fn expect_args(
        &mut self,
        method: &str,
        mc: &MethodCallExpr,
        expected: usize,
    ) -> Result<Vec<Value>> {
        if mc.args.len() != expected {
            return Err(arg_count_error(method, expected, mc.args.len()));
        }
        self.eval_args(&mc.args)
    }

    /// Executes a loop's else-block if the loop did not `break`.
    fn exec_loop_else(&mut self, broke: bool, else_block: &Option<Block>) -> Result<StmtResult> {
        if !broke && let Some(ref block) = *else_block {
            self.env.push_scope();
            let result = self.exec_block(block)?;
            self.env.pop_scope();
            if let StmtResult::Return(_) = result {
                return Ok(result);
            }
        }
        Ok(StmtResult::Continue)
    }

    fn exec_if_inner(&mut self, if_stmt: &IfStmt, implicit_return: bool) -> Result<StmtResult> {
        let condition = self.eval_expr(&if_stmt.condition)?;
        if condition.is_truthy() {
            self.env.push_scope();
            let result = self.exec_block_inner(&if_stmt.then_block, implicit_return)?;
            self.env.pop_scope();
            Ok(result)
        } else if let Some(ref else_branch) = if_stmt.else_branch {
            match else_branch {
                ElseBranch::Block(block) => {
                    self.env.push_scope();
                    let result = self.exec_block_inner(block, implicit_return)?;
                    self.env.pop_scope();
                    Ok(result)
                }
                ElseBranch::ElseIf(elif) => self.exec_if_inner(elif, implicit_return),
            }
        } else {
            Ok(StmtResult::Continue)
        }
    }

    /// Executes a `while` loop. On each iteration the condition is
    /// re-evaluated; the loop exits when the condition is falsy **or**
    /// when a `break` statement is encountered. A `continue` statement
    /// skips the remaining body and proceeds to the next condition check.
    fn exec_while(&mut self, w: &WhileStmt) -> Result<StmtResult> {
        let mut broke = false;
        loop {
            let condition = self.eval_expr(&w.condition)?;
            if !condition.is_truthy() {
                break;
            }
            self.env.push_scope();
            let result = self.exec_block(&w.body)?;
            self.env.pop_scope();
            match result {
                StmtResult::Break => {
                    broke = true;
                    break;
                }
                StmtResult::Return(_) => return Ok(result),
                StmtResult::LoopContinue | StmtResult::Continue => {}
            }
        }
        self.exec_loop_else(broke, &w.else_block)
    }

    /// Executes a `for` loop over a range or collection.
    fn exec_for(&mut self, f: &ForStmt) -> Result<StmtResult> {
        let items: Vec<Value> = match &f.source {
            ForSource::Range { start, end } => {
                let start_val = self.eval_expr(start)?;
                let end_val = self.eval_expr(end)?;
                match (&start_val, &end_val) {
                    (Value::Int(s), Value::Int(e)) => (*s..*e).map(Value::Int).collect(),
                    _ => return error("for loop range must be integers"),
                }
            }
            ForSource::Iterable(iter_expr) => {
                let collection = self.eval_expr(iter_expr)?;
                match collection {
                    Value::List(elements) => elements,
                    _ => return error("for...in requires a List"),
                }
            }
        };

        let mut broke = false;
        for item in items {
            self.env.push_scope();
            self.env.define(f.var_name.clone(), item);
            let result = self.exec_block(&f.body)?;
            self.env.pop_scope();
            match result {
                StmtResult::Break => {
                    broke = true;
                    break;
                }
                StmtResult::Return(_) => return Ok(result),
                StmtResult::LoopContinue | StmtResult::Continue => {}
            }
        }
        self.exec_loop_else(broke, &f.else_block)
    }

    /// Evaluates an expression AST node and returns the resulting runtime [`Value`].
    pub(crate) fn eval_expr(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Literal(lit) => Ok(match &lit.kind {
                LiteralKind::Int(n) => Value::Int(*n),
                LiteralKind::Float(n) => Value::Float(*n),
                LiteralKind::String(s) => Value::String(s.clone()),
                LiteralKind::Bool(b) => Value::Bool(*b),
            }),

            Expr::Ident(ident) => {
                // Check if it's an enum variant with no fields
                if let Some(enum_name) = self.variant_to_enum.get(&ident.name).cloned() {
                    return Ok(Value::EnumVariant(enum_name, ident.name.clone(), vec![]));
                }
                self.env.get(&ident.name).ok_or_else(|| RuntimeError {
                    message: format!("undefined variable `{}`", ident.name),
                    try_return_value: None,
                })
            }

            Expr::Binary(binary) => self.eval_binary(binary),
            Expr::Unary(unary) => self.eval_unary(unary),
            Expr::Call(call) => self.eval_call(call),

            Expr::Assignment(assign) => {
                let value = self.eval_expr(&assign.value)?;
                if !self.env.set(&assign.name, value.clone()) {
                    return error(format!("undefined variable `{}`", assign.name));
                }
                Ok(value)
            }

            // Field assignment: evaluates the right-hand side, then delegates
            // to `assign_field` which handles both direct and nested struct
            // field mutation. Returns the assigned value.
            Expr::FieldAssignment(fa) => {
                let value = self.eval_expr(&fa.value)?;
                self.assign_field(&fa.object, &fa.field, value.clone())?;
                Ok(value)
            }

            Expr::FieldAccess(fa) => {
                let obj = self.eval_expr(&fa.object)?;
                match obj {
                    Value::Struct(_, ref fields) => {
                        fields.get(&fa.field).cloned().ok_or_else(|| RuntimeError {
                            message: format!("no field `{}`", fa.field),
                            try_return_value: None,
                        })
                    }
                    _ => error(format!("cannot access field on {}", obj.type_name())),
                }
            }

            Expr::MethodCall(mc) => self.eval_method_call(mc),
            Expr::StructLiteral(sl) => self.eval_struct_or_variant(sl),
            Expr::Match(m) => self.eval_match(m),

            Expr::ListLiteral(list) => {
                let elements: Vec<Value> = list
                    .elements
                    .iter()
                    .map(|e| self.eval_expr(e))
                    .collect::<Result<_>>()?;
                Ok(Value::List(elements))
            }

            Expr::MapLiteral(map) => {
                let entries: Vec<(Value, Value)> = map
                    .entries
                    .iter()
                    .map(|(k, v)| Ok((self.eval_expr(k)?, self.eval_expr(v)?)))
                    .collect::<Result<_>>()?;
                Ok(Value::Map(entries))
            }

            Expr::Try(try_expr) => self.eval_try(try_expr),

            // String interpolation: concatenates literal segments and the
            // display-formatted results of embedded expression segments into
            // a single String value.
            Expr::StringInterpolation(interp) => {
                let mut result = String::new();
                for segment in &interp.segments {
                    match segment {
                        StringSegment::Literal(s) => result.push_str(s),
                        StringSegment::Expr(expr) => {
                            let value = self.eval_expr(expr)?;
                            result.push_str(&value.to_string());
                        }
                    }
                }
                Ok(Value::String(result))
            }

            Expr::Lambda(lambda) => {
                let params: Vec<String> = lambda.params.iter().map(|p| p.name.clone()).collect();
                // Use sema-provided captures if available, otherwise compute
                // free variables from the AST (needed when interpreter-only
                // unit tests that skip the sema pass).
                let capture_names: Vec<String> =
                    if let Some(sema_captures) = self.lambda_captures.get(&lambda.span) {
                        sema_captures.iter().map(|c| c.name.clone()).collect()
                    } else {
                        phoenix_parser::free_vars::collect_free_variables(&lambda.body, &params)
                            .into_iter()
                            .collect()
                    };
                let mut captures = HashMap::new();
                for name in capture_names {
                    if let Some(cell) = self.env.get_cell(&name) {
                        captures.insert(name, cell);
                    }
                }
                Ok(Value::Closure {
                    params,
                    body: lambda.body.clone(),
                    captures,
                })
            }
        }
    }

    /// Evaluates a `match` expression by testing each arm's pattern against the subject.
    fn eval_match(&mut self, m: &MatchExpr) -> Result<Value> {
        let subject = self.eval_expr(&m.subject)?;

        for arm in &m.arms {
            if let Some(bindings) = self.match_pattern(&arm.pattern, &subject) {
                self.env.push_scope();
                for (name, value) in bindings {
                    self.env.define(name, value);
                }
                let result = match &arm.body {
                    MatchBody::Expr(e) => self.eval_expr(e)?,
                    MatchBody::Block(b) => {
                        match self.exec_block_implicit(b)? {
                            StmtResult::Return(v) => {
                                if self.last_return_was_explicit {
                                    // Explicit `return` — propagate as a
                                    // function return via try_return_value.
                                    self.env.pop_scope();
                                    return Err(RuntimeError {
                                        message: String::new(),
                                        try_return_value: Some(v),
                                    });
                                }
                                // Implicit return (last expression value)
                                v
                            }
                            StmtResult::Break => {
                                self.pending_control_flow = Some(PendingControlFlow::Break);
                                Value::Void
                            }
                            StmtResult::LoopContinue => {
                                self.pending_control_flow = Some(PendingControlFlow::LoopContinue);
                                Value::Void
                            }
                            StmtResult::Continue => Value::Void,
                        }
                    }
                };
                self.env.pop_scope();
                return Ok(result);
            }
        }

        error("non-exhaustive match: no pattern matched")
    }

    /// Attempts to match a value against a pattern, returning bindings on success.
    fn match_pattern(&self, pattern: &Pattern, value: &Value) -> Option<Vec<(String, Value)>> {
        match pattern {
            Pattern::Wildcard(_) => Some(vec![]),

            Pattern::Binding(name, _) => Some(vec![(name.clone(), value.clone())]),

            Pattern::Literal(lit) => {
                let pat_val = match &lit.kind {
                    LiteralKind::Int(n) => Value::Int(*n),
                    LiteralKind::Float(n) => Value::Float(*n),
                    LiteralKind::String(s) => Value::String(s.clone()),
                    LiteralKind::Bool(b) => Value::Bool(*b),
                };
                if &pat_val == value {
                    Some(vec![])
                } else {
                    None
                }
            }

            Pattern::Variant(vp) => {
                if let Value::EnumVariant(_, variant_name, fields) = value
                    && &vp.variant == variant_name
                {
                    let mut bindings = Vec::new();
                    for (i, binding) in vp.bindings.iter().enumerate() {
                        if binding != "_" && i < fields.len() {
                            bindings.push((binding.clone(), fields[i].clone()));
                        }
                    }
                    return Some(bindings);
                }
                None
            }
        }
    }

    /// Evaluates a method call, dispatching to built-in type methods or
    /// user-defined methods.
    fn eval_method_call(&mut self, mc: &MethodCallExpr) -> Result<Value> {
        let obj = self.eval_expr(&mc.object)?;
        // Built-in type methods — match to move data instead of cloning
        match obj {
            Value::String(s) => return self.eval_string_method(s, mc),
            Value::Map(entries) => return self.eval_map_method(entries, mc),
            Value::List(elements) => return self.eval_list_method(elements, mc),
            Value::EnumVariant(ref enum_name, ref variant, ref fields) => {
                match enum_name.as_str() {
                    "Option" => {
                        if let Some(result) = self.eval_option_method(variant, fields, &obj, mc)? {
                            return Ok(result);
                        }
                    }
                    "Result" => {
                        if let Some(result) = self.eval_result_method(variant, fields, &obj, mc)? {
                            return Ok(result);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        let type_name = obj.type_name().to_string();
        let args = self.eval_args(&mc.args)?;
        self.call_method(&type_name, &mc.method, obj, args)
    }

    /// Evaluates a struct constructor or enum variant constructor.
    fn eval_struct_or_variant(&mut self, sl: &StructLiteralExpr) -> Result<Value> {
        // Check if it's a struct
        if let Some(struct_def) = self.structs.get(&sl.name).cloned() {
            if sl.args.len() != struct_def.field_names.len() {
                return error(format!(
                    "struct `{}` takes {} field(s), got {}",
                    sl.name,
                    struct_def.field_names.len(),
                    sl.args.len()
                ));
            }
            let mut fields = BTreeMap::new();
            for (i, arg) in sl.args.iter().enumerate() {
                let value = self.eval_expr(arg)?;
                fields.insert(struct_def.field_names[i].clone(), value);
            }
            return Ok(Value::Struct(sl.name.clone(), fields));
        }

        // Check if it's an enum variant
        if let Some(enum_name) = self.variant_to_enum.get(&sl.name).cloned() {
            if let Some(enum_def) = self.enums.get(&enum_name)
                && let Some(&expected) = enum_def.variants.get(&sl.name)
                && sl.args.len() != expected
            {
                return error(format!(
                    "variant `{}` takes {} argument(s), got {}",
                    sl.name,
                    expected,
                    sl.args.len()
                ));
            }
            let field_vals = self.eval_args(&sl.args)?;
            return Ok(Value::EnumVariant(enum_name, sl.name.clone(), field_vals));
        }

        error(format!("undefined struct or variant `{}`", sl.name))
    }

    /// Evaluates the `?` (try/propagation) operator on a `Result` or `Option`.
    fn eval_try(&mut self, try_expr: &TryExpr) -> Result<Value> {
        let operand = self.eval_expr(&try_expr.operand)?;
        match &operand {
            Value::EnumVariant(enum_name, variant, fields) => {
                match (enum_name.as_str(), variant.as_str()) {
                    ("Result", "Ok") | ("Option", "Some") => {
                        Ok(fields.first().cloned().unwrap_or(Value::Void))
                    }
                    ("Result", "Err") | ("Option", "None") => Err(RuntimeError {
                        message: String::new(),
                        try_return_value: Some(operand),
                    }),
                    _ => error(format!("unexpected variant `{}` for ? operator", variant)),
                }
            }
            _ => error(format!(
                "the ? operator requires a Result or Option value, got {}",
                operand.type_name()
            )),
        }
    }

    /// Evaluates a binary expression by evaluating both operands and applying the operator.
    ///
    /// Boolean `and`/`or` use short-circuit evaluation: the right operand is
    /// only evaluated when the left operand does not determine the result.
    fn eval_binary(&mut self, binary: &BinaryExpr) -> Result<Value> {
        // Short-circuit: evaluate right side only when needed.
        if binary.op == BinaryOp::And {
            let left = self.eval_expr(&binary.left)?;
            return match left {
                Value::Bool(false) => Ok(Value::Bool(false)),
                Value::Bool(true) => {
                    let right = self.eval_expr(&binary.right)?;
                    match right {
                        Value::Bool(b) => Ok(Value::Bool(b)),
                        _ => error("`&&` requires Bool operands"),
                    }
                }
                _ => error("`&&` requires Bool operands"),
            };
        }
        if binary.op == BinaryOp::Or {
            let left = self.eval_expr(&binary.left)?;
            return match left {
                Value::Bool(true) => Ok(Value::Bool(true)),
                Value::Bool(false) => {
                    let right = self.eval_expr(&binary.right)?;
                    match right {
                        Value::Bool(b) => Ok(Value::Bool(b)),
                        _ => error("`||` requires Bool operands"),
                    }
                }
                _ => error("`||` requires Bool operands"),
            };
        }

        let left = self.eval_expr(&binary.left)?;
        let right = self.eval_expr(&binary.right)?;

        match binary.op {
            BinaryOp::Add => match (&left, &right) {
                (Value::String(a), Value::String(b)) => Ok(Value::String(a.clone() + b)),
                _ => checked_numeric_op(&left, &right, "add", i64::checked_add, |a, b| a + b),
            },
            BinaryOp::Sub => {
                checked_numeric_op(&left, &right, "subtract", i64::checked_sub, |a, b| a - b)
            }
            BinaryOp::Mul => {
                checked_numeric_op(&left, &right, "multiply", i64::checked_mul, |a, b| a * b)
            }
            BinaryOp::Div => match (&left, &right) {
                (Value::Int(_), Value::Int(0)) => error("division by zero"),
                (Value::Float(_), Value::Float(b)) if *b == 0.0 => error("division by zero"),
                _ => checked_numeric_op(&left, &right, "divide", i64::checked_div, |a, b| a / b),
            },
            BinaryOp::Mod => match (&left, &right) {
                (Value::Int(_), Value::Int(0)) => error("modulo by zero"),
                (Value::Float(_), Value::Float(b)) if *b == 0.0 => error("modulo by zero"),
                _ => checked_numeric_op(&left, &right, "modulo", i64::checked_rem, |a, b| a % b),
            },
            BinaryOp::Eq => Ok(Value::Bool(left == right)),
            BinaryOp::NotEq => Ok(Value::Bool(left != right)),
            BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq => {
                compare_values(&left, &right, binary.op)
            }
            // And/Or are handled above with short-circuit evaluation.
            BinaryOp::And | BinaryOp::Or => unreachable!(),
        }
    }

    /// Evaluates a unary expression (negation or logical not).
    fn eval_unary(&mut self, unary: &UnaryExpr) -> Result<Value> {
        let operand = self.eval_expr(&unary.operand)?;
        match unary.op {
            UnaryOp::Neg => match operand {
                Value::Int(n) => Ok(Value::Int(n.checked_neg().ok_or_else(|| RuntimeError {
                    message: "integer overflow".to_string(),
                    try_return_value: None,
                })?)),
                Value::Float(n) => Ok(Value::Float(-n)),
                _ => error(format!("cannot negate {}", operand.type_name())),
            },
            UnaryOp::Not => match operand {
                Value::Bool(b) => Ok(Value::Bool(!b)),
                _ => error(format!("cannot apply `!` to {}", operand.type_name())),
            },
        }
    }

    /// Evaluates a function call, handling built-in functions, named functions,
    /// closures, and lambda calls.
    fn eval_call(&mut self, call: &CallExpr) -> Result<Value> {
        // Check for built-in functions first
        if let Expr::Ident(ident) = &call.callee {
            match ident.name.as_str() {
                "print" => {
                    if call.args.len() != 1 {
                        return Err(arg_count_error("print", 1, call.args.len()));
                    }
                    let value = self.eval_expr(&call.args[0])?;
                    writeln!(self.output, "{}", value).map_err(|e| RuntimeError {
                        message: format!("write error: {e}"),
                        try_return_value: None,
                    })?;
                    return Ok(Value::Void);
                }
                "toString" => {
                    if call.args.len() != 1 {
                        return Err(arg_count_error("toString", 1, call.args.len()));
                    }
                    let value = self.eval_expr(&call.args[0])?;
                    return Ok(Value::String(value.to_string()));
                }
                _ => {}
            }
        }

        // Evaluate named arguments
        let named_args: Vec<(String, Value)> = call
            .named_args
            .iter()
            .map(|(name, expr)| Ok((name.clone(), self.eval_expr(expr)?)))
            .collect::<Result<_>>()?;

        // Check for named function or variable holding a closure
        if let Expr::Ident(ident) = &call.callee {
            // Named function
            if let Some(func) = self.functions.get(&ident.name).cloned() {
                let args = self.eval_args(&call.args)?;
                return self.call_function(&func, args, named_args);
            }

            // Variable holding a closure
            if let Some(val) = self.env.get(&ident.name) {
                let args = self.eval_args(&call.args)?;
                return self.call_closure(val, args);
            }

            return error(format!("undefined function `{}`", ident.name));
        }

        // Non-ident callee — evaluate it (could be a lambda expression)
        let callee_val = self.eval_expr(&call.callee)?;
        let args = self.eval_args(&call.args)?;
        self.call_closure(callee_val, args)
    }

    /// Assigns a value to a struct field, handling nested field access chains.
    ///
    /// For a simple case like `point.x = 10`, this retrieves the struct from
    /// the environment, mutates the field, and writes it back. For nested
    /// chains like `user.address.city = "NYC"`, the method recursively walks
    /// the chain, mutates the innermost field, and propagates the updated
    /// struct back to the root variable.
    fn assign_field(&mut self, object: &Expr, field: &str, value: Value) -> Result<()> {
        match object {
            Expr::Ident(ident) => {
                let mut obj = self.env.get(&ident.name).ok_or_else(|| RuntimeError {
                    message: format!("undefined variable `{}`", ident.name),
                    try_return_value: None,
                })?;
                if let Value::Struct(ref _name, ref mut fields) = obj {
                    fields.insert(field.to_string(), value);
                    if !self.env.set(&ident.name, obj) {
                        return error(format!("undefined variable `{}`", ident.name));
                    }
                    Ok(())
                } else {
                    error(format!("cannot assign field on {}", obj.type_name()))
                }
            }
            Expr::FieldAccess(fa) => {
                // Nested: e.g. user.address.city = value
                // First get the intermediate struct
                let mut parent = self.eval_expr(&fa.object)?;
                if let Value::Struct(ref _name, ref mut fields) = parent {
                    if let Some(inner) = fields.get_mut(&fa.field) {
                        if let Value::Struct(_, inner_fields) = inner {
                            inner_fields.insert(field.to_string(), value);
                        } else {
                            return error(format!("cannot assign field on {}", inner.type_name()));
                        }
                    } else {
                        return error(format!("no field `{}`", fa.field));
                    }
                    // Write updated parent back
                    self.assign_field_value(&fa.object, parent)?;
                    Ok(())
                } else {
                    error(format!("cannot assign field on {}", parent.type_name()))
                }
            }
            _ => error("invalid field assignment target".to_string()),
        }
    }

    /// Writes back a fully-constructed value to its root variable after nested
    /// field mutation. Walks the field access chain to find the root identifier,
    /// then sets the root variable to the updated struct.
    fn assign_field_value(&mut self, object: &Expr, value: Value) -> Result<()> {
        match object {
            Expr::Ident(ident) => {
                if !self.env.set(&ident.name, value) {
                    return error(format!("undefined variable `{}`", ident.name));
                }
                Ok(())
            }
            Expr::FieldAccess(fa) => {
                let mut parent = self.eval_expr(&fa.object)?;
                if let Value::Struct(_, ref mut fields) = parent {
                    fields.insert(fa.field.clone(), value);
                    self.assign_field_value(&fa.object, parent)
                } else {
                    error(format!("cannot assign field on {}", parent.type_name()))
                }
            }
            _ => error("invalid field assignment target".to_string()),
        }
    }

    /// Calls a closure value with the given arguments.
    ///
    /// A fresh environment is created containing only the closure's captured
    /// variable cells (in the base scope) and the parameter bindings (in a
    /// second scope).  Because captures are shared `Rc<RefCell<Value>>` cells,
    /// mutations inside the closure are visible in the enclosing scope and
    /// vice versa.
    pub(crate) fn call_closure(&mut self, callee: Value, args: Vec<Value>) -> Result<Value> {
        match callee {
            Value::Closure {
                params,
                body,
                captures,
            } => {
                self.call_depth += 1;
                if self.call_depth > MAX_CALL_DEPTH {
                    self.call_depth -= 1;
                    return error("stack overflow: maximum recursion depth exceeded");
                }

                if params.len() != args.len() {
                    self.call_depth -= 1;
                    return error(format!(
                        "closure takes {} argument(s), got {}",
                        params.len(),
                        args.len()
                    ));
                }

                // Build a fresh environment with captures in the base scope
                let mut closure_env = Environment::new();
                for (name, cell) in &captures {
                    closure_env.define_cell(name.clone(), Rc::clone(cell));
                }

                // Push a scope for parameter bindings
                closure_env.push_scope();
                for (name, value) in params.iter().zip(args) {
                    closure_env.define(name.clone(), value);
                }

                // Swap in the closure environment, execute, then restore
                let saved = std::mem::replace(&mut self.env, closure_env);
                let result = self.exec_block_implicit(&body);
                self.env = saved;
                self.call_depth -= 1;

                self.unwrap_call_result(result)
            }
            _ => error(format!("cannot call value of type {}", callee.type_name())),
        }
    }
}

/// Interprets a Phoenix program by executing its `main()` function.
///
/// This is the main entry point for the tree-walk interpreter. It registers
/// all declarations (functions, structs, enums, traits, impls), then calls
/// `main()`. Returns `Ok(())` on success, or a [`RuntimeError`] if execution
/// fails (e.g., division by zero, stack overflow, undefined variable).
///
/// `lambda_captures` provides the captured variables for each lambda,
/// as computed by the semantic checker. Pass an empty map if sema was skipped.
///
/// # Examples
///
/// Run a simple program through the full pipeline:
///
/// ```
/// use phoenix_lexer::lexer::tokenize;
/// use phoenix_common::span::SourceId;
/// use phoenix_parser::parser;
/// use phoenix_sema::checker;
/// use phoenix_interp::interpreter;
///
/// let tokens = tokenize("function main() { print(1 + 2) }", SourceId(0));
/// let (program, parse_errors) = parser::parse(&tokens);
/// assert!(parse_errors.is_empty());
/// let check_result = checker::check(&program);
/// assert!(check_result.diagnostics.is_empty());
///
/// let result = interpreter::run(&program, check_result.lambda_captures);
/// assert!(result.is_ok());
/// ```
pub fn run(
    program: &Program,
    lambda_captures: HashMap<Span, Vec<CaptureInfo>>,
) -> std::result::Result<(), RuntimeError> {
    let mut interpreter = Interpreter::new();
    interpreter.lambda_captures = lambda_captures;
    interpreter.run_program(program)
}

/// A `Write` wrapper around `Rc<RefCell<Vec<u8>>>` so the buffer can be
/// shared with (and read after) the interpreter finishes.
struct SharedWriter(Rc<RefCell<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

/// Interprets a Phoenix program and captures all `print()` output as lines.
///
/// Returns `Ok(lines)` on success, where each element is one printed line,
/// or a [`RuntimeError`] if execution fails.
///
/// `lambda_captures` provides the captured variables for each lambda,
/// as computed by the semantic checker. Pass an empty map if sema was skipped.
///
/// # Examples
///
/// Capture the output of a program:
///
/// ```
/// use phoenix_lexer::lexer::tokenize;
/// use phoenix_common::span::SourceId;
/// use phoenix_parser::parser;
/// use phoenix_sema::checker;
/// use phoenix_interp::interpreter;
///
/// let source = r#"
/// function main() {
///   print("hello")
///   print(42)
/// }
/// "#;
/// let tokens = tokenize(source, SourceId(0));
/// let (program, _) = parser::parse(&tokens);
/// let check_result = checker::check(&program);
///
/// let output = interpreter::run_and_capture(&program, check_result.lambda_captures).unwrap();
/// assert_eq!(output, vec!["hello", "42"]);
/// ```
pub fn run_and_capture(
    program: &Program,
    lambda_captures: HashMap<Span, Vec<CaptureInfo>>,
) -> std::result::Result<Vec<String>, RuntimeError> {
    let buffer = Rc::new(RefCell::new(Vec::<u8>::new()));
    let writer = SharedWriter(buffer.clone());
    let mut interpreter = Interpreter::with_output(Box::new(writer));
    interpreter.lambda_captures = lambda_captures;
    interpreter.run_program(program)?;
    let bytes = buffer.borrow();
    let output = String::from_utf8_lossy(&bytes);
    Ok(output.lines().map(String::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_common::span::SourceId;
    use phoenix_lexer::lexer::tokenize;
    use phoenix_parser::parser;

    fn run_source(source: &str) -> std::result::Result<(), RuntimeError> {
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);
        run(&program, HashMap::new())
    }

    #[test]
    fn run_empty_main() {
        run_source("function main() { }").unwrap();
    }

    #[test]
    fn run_simple_print() {
        run_source("function main() { print(42) }").unwrap();
    }

    #[test]
    fn run_while_loop() {
        run_source("function main() { let mut x: Int = 0\n while x < 5 { x = x + 1 }\n print(x) }")
            .unwrap();
    }

    #[test]
    fn run_for_loop() {
        run_source("function main() { let mut sum: Int = 0\n for i in 0..5 { sum = sum + i }\n print(sum) }").unwrap();
    }

    #[test]
    fn run_else_if() {
        run_source(
            "function main() {\n  let x: Int = 2\n  if x == 1 {\n    print(\"one\")\n  } else if x == 2 {\n    print(\"two\")\n  } else {\n    print(\"other\")\n  }\n}"
        ).unwrap();
    }

    #[test]
    fn run_struct() {
        run_source(
            "struct Point {\n  Int x\n  Int y\n}\nfunction main() {\n  let p: Point = Point(3, 4)\n  print(p.x)\n  print(p.y)\n}"
        ).unwrap();
    }

    #[test]
    fn run_method() {
        run_source(
            "struct Counter {\n  Int value\n}\nimpl Counter {\n  function get(self) -> Int {\n    return self.value\n  }\n}\nfunction main() {\n  let c: Counter = Counter(42)\n  print(c.get())\n}"
        ).unwrap();
    }

    #[test]
    fn run_enum_and_match() {
        run_source(
            "enum Color {\n  Red\n  Green\n  Blue\n}\nfunction main() {\n  let c: Color = Red\n  match c {\n    Red -> print(\"red\")\n    Green -> print(\"green\")\n    Blue -> print(\"blue\")\n  }\n}"
        ).unwrap();
    }

    #[test]
    fn run_enum_with_fields() {
        run_source(
            "enum Shape {\n  Circle(Float)\n  Rect(Float, Float)\n}\nfunction main() {\n  let s: Shape = Circle(3.14)\n  match s {\n    Circle(r) -> print(r)\n    Rect(w, h) -> print(w)\n  }\n}"
        ).unwrap();
    }

    #[test]
    fn run_recursive_with_while() {
        run_source(
            "function main() {\n  let mut i: Int = 1\n  let mut result: Int = 1\n  while i <= 5 {\n    result = result * i\n    i = i + 1\n  }\n  print(result)\n}"
        ).unwrap();
    }

    #[test]
    fn run_for_loop_sum() {
        run_source(
            "function main() {\n  let mut sum: Int = 0\n  for i in 1..11 {\n    sum = sum + i\n  }\n  print(sum)\n}"
        ).unwrap();
    }

    #[test]
    fn run_no_main() {
        let result = run_source("function foo() { }");
        assert!(result.is_err());
    }

    #[test]
    fn run_division_by_zero() {
        let result = run_source("function main() { print(1 / 0) }");
        assert!(result.is_err());
    }

    #[test]
    fn run_match_wildcard() {
        run_source(
            "enum Color {\n  Red\n  Green\n  Blue\n}\nfunction main() {\n  let c: Color = Green\n  match c {\n    Red -> print(\"red\")\n    _ -> print(\"other\")\n  }\n}"
        ).unwrap();
    }

    #[test]
    fn run_enum_impl_method() {
        run_source(
            "enum Shape {\n  Circle(Float)\n  Rect(Float, Float)\n}\nimpl Shape {\n  function area(self) -> Float {\n    return match self {\n      Circle(r) -> 3.14 * r * r\n      Rect(w, h) -> w * h\n    }\n  }\n}\nfunction main() {\n  let s: Shape = Circle(5.0)\n  print(s.area())\n}"
        ).unwrap();
    }

    #[test]
    fn run_break_in_while() {
        run_source(
            "function main() { let mut x: Int = 0\n while true { x = x + 1\n if x == 5 { break } }\n print(x) }",
        ).unwrap();
    }

    #[test]
    fn run_continue_in_while() {
        run_source(
            "function main() { let mut sum: Int = 0\n let mut i: Int = 0\n while i < 10 { i = i + 1\n if i % 2 == 0 { continue }\n sum = sum + i }\n print(sum) }",
        ).unwrap();
    }

    #[test]
    fn run_break_in_for() {
        run_source(
            "function main() { let mut last: Int = 0\n for i in 0..100 { if i == 5 { break }\n last = i }\n print(last) }",
        ).unwrap();
    }

    #[test]
    fn run_continue_in_for() {
        run_source(
            "function main() { let mut sum: Int = 0\n for i in 0..10 { if i % 2 == 0 { continue }\n sum = sum + i }\n print(sum) }",
        ).unwrap();
    }

    #[test]
    fn run_enum_impl_shared_method() {
        run_source(
            "enum Color {\n  Red\n  Green\n  Blue\n}\nimpl Color {\n  function is_warm(self) -> Bool {\n    return match self {\n      Red -> true\n      _ -> false\n    }\n  }\n}\nfunction main() {\n  let r: Color = Red\n  let b: Color = Blue\n  print(r.is_warm())\n  print(b.is_warm())\n}"
        ).unwrap();
    }

    /// A basic lambda can be created, stored in a variable, and called.
    #[test]
    fn run_lambda_basic() {
        run_source(
            "function main() {\n  let double: (Int) -> Int = function(x: Int) -> Int { return x * 2 }\n  print(double(5))\n}"
        ).unwrap();
    }

    /// A lambda can be passed as an argument to a higher-order function.
    #[test]
    fn run_lambda_as_argument() {
        run_source(
            "function apply(f: (Int) -> Int, x: Int) -> Int {\n  return f(x)\n}\nfunction main() {\n  let triple: (Int) -> Int = function(x: Int) -> Int { return x * 3 }\n  print(apply(triple, 4))\n}"
        ).unwrap();
    }

    /// A closure captures variables from its enclosing scope (make_adder pattern).
    #[test]
    fn run_closure_capture() {
        run_source(
            "function make_adder(n: Int) -> (Int) -> Int {\n  return function(x: Int) -> Int { return x + n }\n}\nfunction main() {\n  let add5: (Int) -> Int = make_adder(5)\n  print(add5(10))\n}"
        ).unwrap();
    }

    /// A lambda can be created inline and passed directly to a function.
    #[test]
    fn run_lambda_inline_call() {
        run_source(
            "function apply(f: (Int) -> Int, x: Int) -> Int {\n  return f(x)\n}\nfunction main() {\n  let result: Int = apply(function(x: Int) -> Int { return x + 1 }, 9)\n  print(result)\n}"
        ).unwrap();
    }

    /// A generic identity function works with both Int and String values
    /// at runtime (type erasure means no runtime type checks).
    #[test]
    fn run_generic_identity() {
        run_source(
            "function identity<T>(x: T) -> T { return x }\nfunction main() {\n  let a: Int = identity(42)\n  print(a)\n  let b: String = identity(\"hello\")\n  print(b)\n}"
        ).unwrap();
    }

    /// A generic struct can be constructed and its fields accessed at runtime.
    #[test]
    fn run_generic_struct() {
        run_source(
            "struct Pair<A, B> {\n  A first\n  B second\n}\nfunction main() {\n  let p: Pair<Int, String> = Pair(1, \"hello\")\n  print(p.first)\n  print(p.second)\n}"
        ).unwrap();
    }

    /// A generic enum with `Some` and `None` variants works with match at runtime.
    #[test]
    fn run_generic_enum_match() {
        run_source(
            "enum Option<T> {\n  Some(T)\n  None\n}\nfunction main() {\n  let a: Option<Int> = Some(42)\n  match a {\n    Some(v) -> print(v)\n    None -> print(0)\n  }\n  let b: Option<Int> = None\n  match b {\n    Some(v) -> print(v)\n    None -> print(0)\n  }\n}"
        ).unwrap();
    }

    #[test]
    fn run_list_literal() {
        run_source("function main() { let nums: List<Int> = [1, 2, 3]\n print(nums) }").unwrap();
    }

    #[test]
    fn run_list_empty() {
        run_source("function main() { let nums: List<Int> = []\n print(nums) }").unwrap();
    }

    #[test]
    fn run_list_length() {
        run_source("function main() { let nums: List<Int> = [10, 20, 30]\n print(nums.length()) }")
            .unwrap();
    }

    #[test]
    fn run_list_get() {
        run_source("function main() { let nums: List<Int> = [10, 20, 30]\n print(nums.get(1)) }")
            .unwrap();
    }

    #[test]
    fn run_list_get_out_of_bounds() {
        let result =
            run_source("function main() { let nums: List<Int> = [1]\n print(nums.get(5)) }");
        assert!(result.is_err());
    }

    #[test]
    fn run_list_push() {
        run_source("function main() { let nums: List<Int> = [1, 2]\n let nums2: List<Int> = nums.push(3)\n print(nums2) }").unwrap();
    }

    #[test]
    fn run_list_iterate_with_while() {
        run_source(
            "function main() {\n  let nums: List<Int> = [10, 20, 30]\n  let mut i: Int = 0\n  let mut sum: Int = 0\n  while i < nums.length() {\n    sum = sum + nums.get(i)\n    i = i + 1\n  }\n  print(sum)\n}"
        ).unwrap();
    }

    /// A generic higher-order function (map) that takes a value and a closure,
    /// applying the closure to transform the value.
    #[test]
    fn run_generic_function_with_closure() {
        run_source(
            "function map<T, U>(x: T, f: (T) -> U) -> U {\n  return f(x)\n}\nfunction main() {\n  let result: String = map(42, function(n: Int) -> String { return toString(n) })\n  print(result)\n}"
        ).unwrap();
    }

    // --- Built-in Option and Result runtime tests ---

    /// `Some(42).unwrap()` returns 42.
    #[test]
    fn run_option_some_unwrap() {
        run_source("function main() {\n  let x: Option<Int> = Some(42)\n  print(x.unwrap())\n}")
            .unwrap();
    }

    /// `None.unwrap()` produces a runtime error.
    #[test]
    fn run_option_none_unwrap_panics() {
        let result =
            run_source("function main() {\n  let x: Option<Int> = None\n  print(x.unwrap())\n}");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("unwrap()"));
    }

    /// `None.unwrapOr(10)` returns 10.
    #[test]
    fn run_option_unwrap_or() {
        run_source("function main() {\n  let x: Option<Int> = None\n  print(x.unwrapOr(10))\n}")
            .unwrap();
    }

    /// `Ok(42).unwrap()` returns 42.
    #[test]
    fn run_result_ok_unwrap() {
        run_source(
            "function main() {\n  let x: Result<Int, String> = Ok(42)\n  print(x.unwrap())\n}",
        )
        .unwrap();
    }

    /// `Err("fail").unwrap()` produces a runtime error.
    #[test]
    fn run_result_err_unwrap_panics() {
        let result = run_source(
            "function main() {\n  let x: Result<Int, String> = Err(\"fail\")\n  print(x.unwrap())\n}",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("unwrap()"));
    }

    /// `Ok(1).isOk()` returns true, `Ok(1).isErr()` returns false.
    #[test]
    fn run_result_is_ok() {
        run_source(
            "function main() {\n  let x: Result<Int, String> = Ok(1)\n  print(x.isOk())\n  print(x.isErr())\n}"
        ).unwrap();
    }

    /// Match on Option with Some/None arms.
    #[test]
    fn run_option_match() {
        run_source(
            "function main() {\n  let x: Option<Int> = Some(42)\n  match x {\n    Some(v) -> print(v)\n    None -> print(0)\n  }\n}"
        ).unwrap();
    }

    /// A closure that returns another closure; both levels capture correctly.
    #[test]
    fn run_nested_closure() {
        run_source(
            "function main() {\n  let base: Int = 10\n  let make: (Int) -> (Int) -> Int = function(a: Int) -> (Int) -> Int {\n    return function(b: Int) -> Int { return base + a + b }\n  }\n  let add5: (Int) -> Int = make(5)\n  print(add5(3))\n}"
        ).unwrap();
    }

    /// A struct with a list field can be constructed and its list accessed.
    #[test]
    fn run_list_in_struct() {
        run_source(
            "struct Bag {\n  String label\n  List<Int> items\n}\nfunction main() {\n  let b: Bag = Bag(\"nums\", [10, 20, 30])\n  print(b.label)\n  print(b.items.get(1))\n}"
        ).unwrap();
    }

    /// `Option<List<Int>>` with Some([1,2,3]) can be matched and the list accessed.
    #[test]
    fn run_option_with_list() {
        run_source(
            "function main() {\n  let x: Option<List<Int>> = Some([1, 2, 3])\n  match x {\n    Some(nums) -> print(nums.get(0))\n    None -> print(0)\n  }\n}"
        ).unwrap();
    }

    /// `list.get(-1)` produces a runtime error (negative index out of bounds).
    #[test]
    fn run_list_negative_index() {
        let result =
            run_source("function main() { let nums: List<Int> = [1, 2, 3]\n print(nums.get(-1)) }");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("out of bounds"));
    }

    /// A function that returns a closure; the returned closure is called successfully.
    #[test]
    fn run_closure_as_return_value() {
        run_source(
            "function make_multiplier(factor: Int) -> (Int) -> Int {\n  return function(x: Int) -> Int { return x * factor }\n}\nfunction main() {\n  let times3: (Int) -> Int = make_multiplier(3)\n  print(times3(7))\n}"
        ).unwrap();
    }

    /// Float division by zero returns a runtime error.
    #[test]
    fn run_float_division_by_zero() {
        let result = run_source("function main() { print(1.0 / 0.0) }");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("division by zero"));
    }

    /// Float modulo by zero returns a runtime error.
    #[test]
    fn run_float_modulo_by_zero() {
        let result = run_source("function main() { print(1.0 % 0.0) }");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("modulo by zero"));
    }

    /// Infinite recursion is caught with a stack overflow error.
    #[test]
    fn run_infinite_recursion_caught() {
        let result = run_source("function boom() { boom() }\nfunction main() { boom() }");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("stack overflow"));
    }

    /// Mutual recursion that overflows is caught.
    #[test]
    fn run_mutual_recursion_caught() {
        let result =
            run_source("function a() { b() }\nfunction b() { a() }\nfunction main() { a() }");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("stack overflow"));
    }

    /// String escape sequences are processed correctly.
    #[test]
    fn run_string_escape_sequences() {
        // Literal backslash-n should remain as backslash + n, not become a newline
        run_source(
            r#"function main() { let s: String = "hello\nworld"
print(s) }"#,
        )
        .unwrap();
    }

    /// Literal backslash in strings is handled correctly.
    #[test]
    fn run_string_literal_backslash() {
        run_source(
            r#"function main() { let s: String = "a\\b"
print(s) }"#,
        )
        .unwrap();
    }

    /// Match expression with block body executes correctly.
    #[test]
    fn run_match_with_block_body() {
        run_source(
            r#"
enum Shape {
  Circle(Float)
  Rect(Float, Float)
}
function main() {
  let s: Shape = Rect(3.0, 4.0)
  let result: String = match s {
    Circle(r) -> "circle"
    Rect(w, h) -> {
      if w == h { return "square" }
      return "rectangle"
    }
  }
  print(result)
}"#,
        )
        .unwrap();
    }

    /// Deeply nested generics work at runtime.
    #[test]
    fn run_deeply_nested_generics() {
        run_source(
            r#"
function main() {
  let x: Option<List<Int>> = Some([1, 2, 3])
  match x {
    Some(nums) -> print(nums.get(0))
    None -> print(0)
  }
  let items: List<Option<Int>> = [Some(1), None, Some(3)]
  print(items.get(0))
}"#,
        )
        .unwrap();
    }

    /// Closures capturing closures (3 levels deep) work correctly.
    #[test]
    fn run_triple_nested_closure() {
        run_source(
            r#"
function main() {
  let a: Int = 1
  let f: (Int) -> (Int) -> (Int) -> Int = function(b: Int) -> (Int) -> (Int) -> Int {
    return function(c: Int) -> (Int) -> Int {
      return function(d: Int) -> Int {
        return a + b + c + d
      }
    }
  }
  let g: (Int) -> (Int) -> Int = f(2)
  let h: (Int) -> Int = g(3)
  print(h(4))
}"#,
        )
        .unwrap();
    }

    // --- Phase 1.8 feature tests ---

    /// Field assignment basic case.
    #[test]
    fn run_field_assignment_basic() {
        run_source(
            r#"
struct Point {
  Int x
  Int y
}
function main() {
  let mut p: Point = Point(1, 2)
  p.x = 10
  print(p.x)
}"#,
        )
        .unwrap();
    }

    /// Field assignment nested.
    #[test]
    fn run_field_assignment_nested() {
        run_source(
            r#"
struct Inner { Int value }
struct Outer { Inner inner }
function main() {
  let mut o: Outer = Outer(Inner(1))
  o.inner.value = 42
  print(o.inner.value)
}"#,
        )
        .unwrap();
    }

    /// String interpolation runtime output.
    #[test]
    fn run_string_interpolation() {
        run_source(
            r#"
function main() {
  let name: String = "world"
  let greeting: String = "hello {name}"
  print(greeting)
}"#,
        )
        .unwrap();
    }

    /// `?` operator with Ok unwraps the value.
    #[test]
    fn run_try_operator_ok() {
        run_source(
            r#"
function get_value() -> Result<Int, String> {
  return Ok(42)
}
function compute() -> Result<Int, String> {
  let v: Int = get_value()?
  return Ok(v + 1)
}
function main() {
  let r: Result<Int, String> = compute()
  print(r)
}"#,
        )
        .unwrap();
    }

    /// `?` operator with Err propagates the error.
    #[test]
    fn run_try_operator_err() {
        run_source(
            r#"
function fail() -> Result<Int, String> {
  return Err("oops")
}
function compute() -> Result<Int, String> {
  let v: Int = fail()?
  return Ok(v + 1)
}
function main() {
  let r: Result<Int, String> = compute()
  print(r)
}"#,
        )
        .unwrap();
    }

    /// Implicit return works in functions.
    #[test]
    fn run_implicit_return_function() {
        run_source(
            r#"
function add(a: Int, b: Int) -> Int {
  a + b
}
function main() {
  let result: Int = add(3, 4)
  print(result)
}"#,
        )
        .unwrap();
    }

    /// Implicit return works in closures.
    #[test]
    fn run_implicit_return_closure() {
        run_source(
            r#"
function main() {
  let double: (Int) -> Int = function(x: Int) -> Int { x * 2 }
  print(double(5))
}"#,
        )
        .unwrap();
    }

    // Helper that captures printed output as lines.
    fn run_capturing_source(source: &str) -> Vec<String> {
        let tokens = tokenize(source, SourceId(0));
        let (program, errors) = parser::parse(&tokens);
        assert!(errors.is_empty(), "parse errors: {:?}", errors);
        run_and_capture(&program, HashMap::new()).unwrap()
    }

    // ---- List higher-order method tests ----

    /// `[1, 2, 3].map(fn)` doubles each element.
    #[test]
    fn run_list_map() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let doubled: List<Int> = nums.map(function(x: Int) -> Int { x * 2 })
  print(doubled)
}"#,
        );
        assert_eq!(output, vec!["[2, 4, 6]"]);
    }

    /// `[1, 2, 3, 4, 5].filter(fn)` keeps elements greater than 2.
    #[test]
    fn run_list_filter() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3, 4, 5]
  let filtered: List<Int> = nums.filter(function(x: Int) -> Bool { x > 2 })
  print(filtered)
}"#,
        );
        assert_eq!(output, vec!["[3, 4, 5]"]);
    }

    /// `[1, 2, 3, 4].reduce(0, fn)` sums all elements.
    #[test]
    fn run_list_reduce() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3, 4]
  let total: Int = nums.reduce(0, function(acc: Int, x: Int) -> Int { acc + x })
  print(total)
}"#,
        );
        assert_eq!(output, vec!["10"]);
    }

    /// `[1, 2, 3].find(fn)` returns Some(2) when predicate matches.
    #[test]
    fn run_list_find_some() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let found: Option<Int> = nums.find(function(x: Int) -> Bool { x == 2 })
  print(found)
}"#,
        );
        assert_eq!(output, vec!["Some(2)"]);
    }

    /// `[1, 2, 3].find(fn)` returns None when no element matches.
    #[test]
    fn run_list_find_none() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let found: Option<Int> = nums.find(function(x: Int) -> Bool { x == 5 })
  print(found)
}"#,
        );
        assert_eq!(output, vec!["None"]);
    }

    /// `[1, 2, 3].any(fn)` returns true when at least one element matches.
    #[test]
    fn run_list_any_true() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let result: Bool = nums.any(function(x: Int) -> Bool { x == 2 })
  print(result)
}"#,
        );
        assert_eq!(output, vec!["true"]);
    }

    /// `[1, 2, 3].any(fn)` returns false when no element matches.
    #[test]
    fn run_list_any_false() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let result: Bool = nums.any(function(x: Int) -> Bool { x == 5 })
  print(result)
}"#,
        );
        assert_eq!(output, vec!["false"]);
    }

    /// `[1, 2, 3].all(fn)` returns true when all elements match.
    #[test]
    fn run_list_all_true() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let result: Bool = nums.all(function(x: Int) -> Bool { x > 0 })
  print(result)
}"#,
        );
        assert_eq!(output, vec!["true"]);
    }

    /// `[1, 2, 3].all(fn)` returns false when not all elements match.
    #[test]
    fn run_list_all_false() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let result: Bool = nums.all(function(x: Int) -> Bool { x > 1 })
  print(result)
}"#,
        );
        assert_eq!(output, vec!["false"]);
    }

    /// `[1, 2, 3].flatMap(fn)` flattens nested lists.
    #[test]
    fn run_list_flat_map() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  let result: List<Int> = nums.flatMap(function(x: Int) -> List<Int> { [x, x * 10] })
  print(result)
}"#,
        );
        assert_eq!(output, vec!["[1, 10, 2, 20, 3, 30]"]);
    }

    /// `[3, 1, 2].sortBy(fn)` sorts by comparator.
    #[test]
    fn run_list_sort_by() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [3, 1, 2]
  let sorted: List<Int> = nums.sortBy(function(a: Int, b: Int) -> Int { a - b })
  print(sorted)
}"#,
        );
        assert_eq!(output, vec!["[1, 2, 3]"]);
    }

    /// `.first()` and `.last()` return Option values.
    #[test]
    fn run_list_first_last() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [10, 20, 30]
  print(nums.first())
  print(nums.last())
}"#,
        );
        assert_eq!(output, vec!["Some(10)", "Some(30)"]);
    }

    /// `.contains(2)` on a list returns true.
    #[test]
    fn run_list_contains() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3]
  print(nums.contains(2))
}"#,
        );
        assert_eq!(output, vec!["true"]);
    }

    /// `.take(3)` and `.drop(2)` return sub-lists.
    #[test]
    fn run_list_take_drop() {
        let output = run_capturing_source(
            r#"
function main() {
  let nums: List<Int> = [1, 2, 3, 4, 5]
  print(nums.take(3))
  print(nums.drop(2))
}"#,
        );
        assert_eq!(output, vec!["[1, 2, 3]", "[3, 4, 5]"]);
    }

    // ---- Map method tests ----

    /// `.set()` adds or updates a key in a map.
    #[test]
    fn run_map_set() {
        let output = run_capturing_source(
            r#"
function main() {
  let mut m: Map<String, Int> = {"a": 1, "b": 2}
  m = m.set("b", 99)
  m = m.set("c", 3)
  print(m)
}"#,
        );
        assert_eq!(output, vec!["{a: 1, b: 99, c: 3}"]);
    }

    /// `.remove()` removes a key from a map.
    #[test]
    fn run_map_remove() {
        let output = run_capturing_source(
            r#"
function main() {
  let mut m: Map<String, Int> = {"a": 1, "b": 2, "c": 3}
  m = m.remove("b")
  print(m)
}"#,
        );
        assert_eq!(output, vec!["{a: 1, c: 3}"]);
    }

    /// `.keys()` and `.values()` return lists.
    #[test]
    fn run_map_keys_values() {
        let output = run_capturing_source(
            r#"
function main() {
  let m: Map<String, Int> = {"x": 10, "y": 20}
  print(m.keys())
  print(m.values())
}"#,
        );
        assert_eq!(output, vec!["[x, y]", "[10, 20]"]);
    }

    // ---- Result/Option combinator tests ----

    /// `Err("bad").mapErr(fn)` transforms the error value.
    #[test]
    fn run_result_map_err() {
        let output = run_capturing_source(
            r#"
function main() {
  let r: Result<Int, String> = Err("bad")
  let mapped: Result<Int, String> = r.mapErr(function(e: String) -> String { "error: " + e })
  print(mapped)
}"#,
        );
        assert_eq!(output, vec!["Err(error: bad)"]);
    }

    /// `.ok()` on Ok returns Some, on Err returns None.
    #[test]
    fn run_result_ok() {
        let output = run_capturing_source(
            r#"
function main() {
  let good: Result<Int, String> = Ok(42)
  let bad: Result<Int, String> = Err("x")
  print(good.ok())
  print(bad.ok())
}"#,
        );
        assert_eq!(output, vec!["Some(42)", "None"]);
    }

    /// `.err()` on Err returns Some, on Ok returns None.
    #[test]
    fn run_result_err() {
        let output = run_capturing_source(
            r#"
function main() {
  let bad: Result<Int, String> = Err("fail")
  let good: Result<Int, String> = Ok(1)
  print(bad.err())
  print(good.err())
}"#,
        );
        assert_eq!(output, vec!["Some(fail)", "None"]);
    }

    // ---- Edge case tests ----

    /// Negating i64::MIN overflows and produces a runtime error.
    #[test]
    fn run_integer_negation_overflow() {
        let result = run_source(
            r#"
function main() {
  let min: Int = -9223372036854775807 - 1
  let boom: Int = -min
  print(boom)
}"#,
        );
        assert!(result.is_err());
        assert!(
            result.unwrap_err().message.contains("overflow"),
            "expected overflow error"
        );
    }

    /// `break` inside a match arm in a loop exits the loop.
    #[test]
    fn run_break_in_match_arm() {
        let output = run_capturing_source(
            r#"
enum Token {
  Num(Int)
  Stop
}
function main() {
  let tokens: List<Token> = [Num(1), Num(2), Stop, Num(3)]
  let mut sum: Int = 0
  let mut i: Int = 0
  while i < tokens.length() {
    let t: Token = tokens.get(i)
    match t {
      Num(n) -> { sum = sum + n }
      Stop -> { break }
    }
    i = i + 1
  }
  print(sum)
}"#,
        );
        assert_eq!(output, vec!["3"]);
    }

    /// Nested field assignment through 3 levels of structs.
    #[test]
    fn run_nested_field_assignment_3_levels() {
        let output = run_capturing_source(
            r#"
struct C { Int value }
struct B { C c }
struct A { B b }
function main() {
  let mut a: A = A(B(C(0)))
  a.b.c.value = 42
  print(a.b.c.value)
}"#,
        );
        assert_eq!(output, vec!["42"]);
    }
}
