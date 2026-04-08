use crate::value::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A shared, reference-counted cell holding a runtime value.
///
/// Variables in the environment are stored behind `Rc<RefCell<…>>` so that
/// closures can capture them by reference: multiple closures (and the
/// enclosing scope) can hold `Rc` handles to the same cell, and mutations
/// through any handle are visible to all others.
pub type ValueCell = Rc<RefCell<Value>>;

/// A stack of variable scopes used by the Phoenix interpreter.
///
/// Scopes form a LIFO stack. Variable look-ups walk from the innermost
/// (most recently pushed) scope outward, so inner definitions shadow
/// outer ones.
///
/// Each variable is stored as a [`ValueCell`] (`Rc<RefCell<Value>>`),
/// enabling closures to share variables with their defining scope by
/// capturing the same `Rc` handle.
#[derive(Debug)]
pub struct Environment {
    scopes: Vec<HashMap<String, ValueCell>>,
}

impl Default for Environment {
    fn default() -> Self {
        Self::new()
    }
}

impl Environment {
    /// Creates a new environment with a single (global) scope.
    pub fn new() -> Self {
        Self {
            scopes: vec![HashMap::new()],
        }
    }

    /// Pushes a fresh, empty scope onto the scope stack.
    ///
    /// Call this when entering a block (function body, if-branch, etc.).
    pub fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    /// Pops the innermost scope, discarding all variables defined in it.
    ///
    /// Call this when leaving a block.
    pub fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Defines (or redefines) a variable in the **current** (innermost) scope,
    /// wrapping the value in a fresh [`ValueCell`].
    pub fn define(&mut self, name: String, value: Value) {
        self.scopes
            .last_mut()
            .expect("scope stack must not be empty")
            .insert(name, Rc::new(RefCell::new(value)));
    }

    /// Inserts an existing [`ValueCell`] into the current scope.
    ///
    /// Used when setting up a closure's execution environment: the captured
    /// cells are placed directly into a scope so that reads and writes go
    /// through the shared cell.
    pub fn define_cell(&mut self, name: String, cell: ValueCell) {
        self.scopes
            .last_mut()
            .expect("scope stack must not be empty")
            .insert(name, cell);
    }

    /// Looks up a variable by name, searching from the innermost scope outward.
    ///
    /// Returns a **clone** of the value inside the cell, or `None` if the
    /// variable is not found in any scope.
    pub fn get(&self, name: &str) -> Option<Value> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).map(|cell| cell.borrow().clone()))
    }

    /// Returns the [`ValueCell`] for a named variable, searching from the
    /// innermost scope outward.
    ///
    /// Used by closure creation to capture a shared reference to a variable.
    pub fn get_cell(&self, name: &str) -> Option<ValueCell> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).map(Rc::clone))
    }

    /// Updates an existing variable in the nearest enclosing scope that
    /// contains it.
    ///
    /// Returns `true` if the variable was found and updated, `false` if
    /// no scope contains a variable with the given name.
    #[must_use]
    pub fn set(&mut self, name: &str, value: Value) -> bool {
        if let Some(cell) = self.scopes.iter().rev().find_map(|scope| scope.get(name)) {
            *cell.borrow_mut() = value;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn define_and_get() {
        let mut env = Environment::new();
        env.define("x".into(), Value::Int(42));
        assert_eq!(env.get("x"), Some(Value::Int(42)));
    }

    #[test]
    fn get_undefined_returns_none() {
        let env = Environment::new();
        assert_eq!(env.get("nope"), None);
    }

    #[test]
    fn set_existing_var() {
        let mut env = Environment::new();
        env.define("x".into(), Value::Int(1));
        assert!(env.set("x", Value::Int(2)));
        assert_eq!(env.get("x"), Some(Value::Int(2)));
    }

    #[test]
    fn set_undefined_returns_false() {
        let mut env = Environment::new();
        assert!(!env.set("missing", Value::Int(0)));
    }

    #[test]
    fn scope_push_pop() {
        let mut env = Environment::new();
        env.push_scope();
        env.define("tmp".into(), Value::Bool(true));
        assert_eq!(env.get("tmp"), Some(Value::Bool(true)));
        env.pop_scope();
        assert_eq!(env.get("tmp"), None);
    }

    #[test]
    fn get_from_parent_scope() {
        let mut env = Environment::new();
        env.define("outer".into(), Value::String("hello".into()));
        env.push_scope();
        assert_eq!(env.get("outer"), Some(Value::String("hello".into())));
        env.pop_scope();
    }

    #[test]
    fn set_in_parent_scope() {
        let mut env = Environment::new();
        env.define("x".into(), Value::Int(1));
        env.push_scope();
        assert!(env.set("x", Value::Int(99)));
        env.pop_scope();
        assert_eq!(env.get("x"), Some(Value::Int(99)));
    }

    #[test]
    fn shadowing() {
        let mut env = Environment::new();
        env.define("x".into(), Value::Int(1));
        env.push_scope();
        env.define("x".into(), Value::Int(2));
        assert_eq!(env.get("x"), Some(Value::Int(2)));
        env.pop_scope();
        assert_eq!(env.get("x"), Some(Value::Int(1)));
    }

    #[test]
    fn define_cell_shares_mutations() {
        let mut env = Environment::new();
        env.define("x".into(), Value::Int(1));
        let cell = env.get_cell("x").unwrap();

        // Mutate through the cell directly
        *cell.borrow_mut() = Value::Int(42);

        // The environment sees the mutation
        assert_eq!(env.get("x"), Some(Value::Int(42)));
    }

    #[test]
    fn define_cell_in_child_scope_shares_with_parent() {
        let mut env = Environment::new();
        env.define("x".into(), Value::Int(1));
        let cell = env.get_cell("x").unwrap();

        // Insert the same cell into a child scope
        env.push_scope();
        env.define_cell("x".into(), Rc::clone(&cell));

        // Mutate via the child scope
        assert!(env.set("x", Value::Int(99)));
        env.pop_scope();

        // Parent scope sees the mutation (same underlying cell)
        assert_eq!(env.get("x"), Some(Value::Int(99)));
    }
}
