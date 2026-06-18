//! The host-FFI exchange types shared by the interpreters.
//!
//! `extern js` is a uniform host-FFI boundary ([design-decisions.md §Phase 2.5
//! decision A0]): a Phoenix program calls a host function, and each backend
//! *binds* that call to its host. The two interpreters (`phoenix-interp`,
//! `phoenix-ir-interp`) share this module so a Rust host stub is written **once**
//! against a backend-neutral marshalled-value type ([`HostValue`]) and the
//! [`HostContext`] callback bridge — never twice, once per interpreter's native
//! value type. Each interpreter marshals its own `Value` / `IrValue` to and from
//! [`HostValue`] at the boundary and implements [`HostContext`] in terms of its
//! own closure-call machinery.
//!
//! This is the interpreter binding of decision A0; the WASM glue (PRs 5–8 /
//! 12–15) and the native C-ABI shim (PR 9) are the other bindings of the same
//! conceptual boundary.

use std::collections::HashMap;

/// A value marshalled across the `extern js` host-FFI boundary.
///
/// The marshallable set mirrors `phoenix_sema::types::Type::is_js_marshallable`:
/// the scalars, `String`, the opaque [`HostValue::JsValue`] handle, `Void`, and
/// a [`HostValue::Callback`] (a Phoenix closure handed to the host). Aggregates
/// never reach here — sema rejects a non-marshallable type at the extern
/// signature, so a marshalling failure at runtime is an internal error, not a
/// user error.
#[derive(Debug, Clone)]
pub enum HostValue {
    /// A 64-bit signed integer (`Int`).
    Int(i64),
    /// A 64-bit float (`Float`).
    Float(f64),
    /// A boolean (`Bool`).
    Bool(bool),
    /// A UTF-8 string (`String`), copied across the boundary.
    Str(String),
    /// An opaque JavaScript-host value handle (`JsValue`). Phoenix never
    /// inspects it; the host owns the real object and Phoenix only round-trips
    /// the handle. In the interpreters the handle space is owned by the host
    /// stub registry.
    JsValue(u64),
    /// A Phoenix closure handed to the host as a callback. The host invokes it
    /// via [`HostContext::call_callback`]; the handle identifies the closure
    /// within the interpreter that produced it.
    Callback(CallbackHandle),
    /// The unit value (`Void`) — a host function's "no result".
    Void,
}

/// An opaque handle to a Phoenix closure the host may invoke as a callback.
///
/// Minted by an interpreter when a closure is marshalled out across the
/// boundary; resolved back to the closure by that same interpreter in
/// [`HostContext::call_callback`]. Opaque to host stubs — they only pass it
/// back through the context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallbackHandle(pub u64);

/// The bridge that lets a host function call **back** into Phoenix.
///
/// Implemented by each interpreter (over its own closure-call machinery) and
/// passed to every [`HostFunction`] invocation. A host stub modelling an async
/// JS API (`setTimeout(cb, ms)`) uses it to invoke the Phoenix callback the
/// program passed in — synchronously, since the interpreters have no event
/// loop (the callbacks-only async model; design-decisions §Phase 2.5 H).
pub trait HostContext {
    /// Invoke a Phoenix callback by handle with marshalled arguments, returning
    /// its marshalled result. `Err` carries a message if the handle is invalid
    /// or the callback itself errors.
    fn call_callback(
        &mut self,
        handle: CallbackHandle,
        args: Vec<HostValue>,
    ) -> Result<HostValue, String>;
}

/// A registered host function: receives the [`HostContext`] (for invoking
/// callbacks) and the marshalled arguments, and returns a marshalled result or
/// an error message. Written once and shared by both interpreters.
pub type HostFunction =
    Box<dyn Fn(&mut dyn HostContext, Vec<HostValue>) -> Result<HostValue, String>>;

/// A registry of host functions keyed by `(module, name)` (Phase 2.5).
///
/// An interpreter holds one (empty by default) and consults it when an extern
/// call fires. An unregistered `(module, name)` is a clean runtime error, never
/// a silent no-op. The embedder / test harness populates it before running a
/// program; the bare CLI registers nothing (binary-level host provisioning is
/// PR 16), so `phoenix run` of an interop program reports the missing binding.
#[derive(Default)]
pub struct HostRegistry {
    // Nested map so lookup borrows `&str` without allocating a tuple key.
    funcs: HashMap<String, HashMap<String, HostFunction>>,
}

impl HostRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `f` as the host binding for `(module, name)`, replacing any
    /// previous binding for that key.
    pub fn register(
        &mut self,
        module: impl Into<String>,
        name: impl Into<String>,
        f: HostFunction,
    ) {
        self.funcs
            .entry(module.into())
            .or_default()
            .insert(name.into(), f);
    }

    /// Look up the host binding for `(module, name)`, or `None` if unregistered.
    #[must_use]
    pub fn get(&self, module: &str, name: &str) -> Option<&HostFunction> {
        self.funcs.get(module)?.get(name)
    }
}

impl std::fmt::Debug for HostRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `HostFunction` is not `Debug`; list the registered keys instead.
        let keys: Vec<String> = self
            .funcs
            .iter()
            .flat_map(|(m, names)| names.keys().map(move |n| format!("{m}.{n}")))
            .collect();
        f.debug_struct("HostRegistry")
            .field("bindings", &keys)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_get() {
        let mut reg = HostRegistry::new();
        assert!(reg.get("js", "answer").is_none());
        reg.register(
            "js",
            "answer",
            Box::new(|_ctx, _args| Ok(HostValue::Int(42))),
        );
        assert!(reg.get("js", "answer").is_some());
        assert!(reg.get("js", "missing").is_none());
        assert!(reg.get("other", "answer").is_none());
    }

    #[test]
    fn register_replaces_prior_binding() {
        let mut reg = HostRegistry::new();
        reg.register("js", "f", Box::new(|_c, _a| Ok(HostValue::Int(1))));
        reg.register("js", "f", Box::new(|_c, _a| Ok(HostValue::Int(2))));
        // A tiny dummy context: no callbacks exercised here.
        struct NoCtx;
        impl HostContext for NoCtx {
            fn call_callback(
                &mut self,
                _h: CallbackHandle,
                _a: Vec<HostValue>,
            ) -> Result<HostValue, String> {
                Err("no callbacks".into())
            }
        }
        let f = reg.get("js", "f").unwrap();
        match f(&mut NoCtx, vec![]) {
            Ok(HostValue::Int(2)) => {}
            other => panic!("expected the second binding (Int(2)), got {other:?}"),
        }
    }
}
