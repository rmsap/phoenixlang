use crate::types::Type;
use std::collections::HashMap;

/// Information about a variable binding in a lexical scope.
///
/// Each variable tracked by the scope stack carries its resolved [`Type`]
/// and a flag indicating whether the binding was declared as mutable
/// (`mut`).  Phoenix uses garbage collection for memory management, so no
/// ownership or move tracking is needed at the semantic analysis level.
#[derive(Debug, Clone)]
pub struct VarInfo {
    /// The resolved type of the variable.
    pub ty: Type,
    /// Whether the variable was declared with the `mut` qualifier.
    pub is_mut: bool,
}

/// A lexical scope containing variable bindings.
#[derive(Debug)]
struct Scope {
    vars: HashMap<String, VarInfo>,
}

/// A stack of lexical scopes for name resolution.
///
/// `ScopeStack` models the nested block structure of a Phoenix program.
/// Variables are defined in the current (innermost) scope and looked up
/// from innermost to outermost, which naturally implements lexical
/// shadowing.
#[derive(Debug)]
pub struct ScopeStack {
    scopes: Vec<Scope>,
}

impl Default for ScopeStack {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopeStack {
    /// Creates a new scope stack with a single empty global scope.
    pub fn new() -> Self {
        Self {
            scopes: vec![Scope {
                vars: HashMap::new(),
            }],
        }
    }

    /// Pushes a new empty scope onto the stack (e.g. entering a block or function body).
    pub fn push(&mut self) {
        self.scopes.push(Scope {
            vars: HashMap::new(),
        });
    }

    /// Pops the innermost scope, discarding all variable bindings it contains.
    pub fn pop(&mut self) {
        self.scopes.pop();
    }

    /// Defines a variable in the current scope.
    ///
    /// Returns `false` if a variable with the same name is already defined
    /// in the current (innermost) scope, leaving the existing definition
    /// unchanged.
    pub fn define(&mut self, name: String, info: VarInfo) -> bool {
        let scope = self
            .scopes
            .last_mut()
            .expect("scope stack must not be empty");
        if scope.vars.contains_key(&name) {
            return false;
        }
        scope.vars.insert(name, info);
        true
    }

    /// Looks up a variable by name, searching from innermost to outermost scope.
    ///
    /// Returns `None` if the variable has not been defined in any enclosing
    /// scope.
    pub fn lookup(&self, name: &str) -> Option<&VarInfo> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.vars.get(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a simple `VarInfo`.
    fn var(ty: Type, is_mut: bool) -> VarInfo {
        VarInfo { ty, is_mut }
    }

    #[test]
    fn define_and_lookup() {
        let mut stack = ScopeStack::new();
        assert!(stack.define("x".into(), var(Type::Int, false)));
        let info = stack.lookup("x").expect("should find x");
        assert_eq!(info.ty, Type::Int);
        assert!(!info.is_mut);
    }

    #[test]
    fn lookup_undefined_returns_none() {
        let stack = ScopeStack::new();
        assert!(stack.lookup("nope").is_none());
    }

    #[test]
    fn duplicate_in_same_scope_returns_false() {
        let mut stack = ScopeStack::new();
        assert!(stack.define("x".into(), var(Type::Int, false)));
        assert!(!stack.define("x".into(), var(Type::Float, true)));
        // The original definition is kept.
        assert_eq!(stack.lookup("x").unwrap().ty, Type::Int);
    }

    #[test]
    fn shadowing_across_scopes() {
        let mut stack = ScopeStack::new();
        stack.define("x".into(), var(Type::Int, false));

        stack.push();
        stack.define("x".into(), var(Type::String, true));
        assert_eq!(stack.lookup("x").unwrap().ty, Type::String);

        stack.pop();
        // Outer definition is restored.
        assert_eq!(stack.lookup("x").unwrap().ty, Type::Int);
    }

    #[test]
    fn lookup_in_parent_scope() {
        let mut stack = ScopeStack::new();
        stack.define("outer".into(), var(Type::Bool, false));

        stack.push();
        // Should still be visible from the inner scope.
        let info = stack.lookup("outer").expect("should find outer");
        assert_eq!(info.ty, Type::Bool);
    }

    #[test]
    fn pop_removes_inner_vars() {
        let mut stack = ScopeStack::new();
        stack.define("outer".into(), var(Type::Int, false));

        stack.push();
        stack.define("inner".into(), var(Type::Float, true));
        assert!(stack.lookup("inner").is_some());

        stack.pop();
        assert!(stack.lookup("inner").is_none());
        assert!(stack.lookup("outer").is_some());
    }

    #[test]
    fn multiple_nested_scopes() {
        let mut stack = ScopeStack::new();
        stack.define("a".into(), var(Type::Int, false));

        stack.push();
        stack.define("b".into(), var(Type::Float, false));

        stack.push();
        stack.define("c".into(), var(Type::String, false));

        // All three visible.
        assert_eq!(stack.lookup("a").unwrap().ty, Type::Int);
        assert_eq!(stack.lookup("b").unwrap().ty, Type::Float);
        assert_eq!(stack.lookup("c").unwrap().ty, Type::String);

        stack.pop();
        assert!(stack.lookup("c").is_none());
        assert!(stack.lookup("b").is_some());

        stack.pop();
        assert!(stack.lookup("b").is_none());
        assert!(stack.lookup("a").is_some());
    }
}
