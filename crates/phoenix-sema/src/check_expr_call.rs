use crate::check_types::UnifyError;
use crate::checker::{Checker, FunctionInfo};
use crate::module_scope::{IntrinsicNs, NamespaceTarget};
use crate::types::{OPTION_ENUM, Type};
use phoenix_common::module_path::{ModulePath, module_qualify};
use phoenix_common::span::Span;
use phoenix_parser::ast::{CallExpr, Expr, MethodCallExpr, StructLiteralExpr, Visibility};
use std::collections::{HashMap, HashSet};

/// Output of [`Checker::infer_and_check_call_generics`]:
/// `(bindings, arg_types, errors)`.
/// See that method's docstring for field semantics.
type CallGenericsInference = (HashMap<String, Type>, Vec<Type>, Vec<(usize, UnifyError)>);

impl Checker {
    /// Dispatches method `mc` against a built-in receiver type (`List`, `String`,
    /// `Map`, `Option`, `Result`, `ListBuilder`, `MapBuilder`), returning the result
    /// type — or `None` when `obj_type` is not one of those built-ins (a user/`dyn`
    /// type hits the `_ => None` arm).
    ///
    /// For an unrecognized *method* the arms differ: the `Option`/`Result` arms
    /// return `None` silently (no diagnostic), whereas the `String`/`List`/`Map`
    /// arms report "no method … on type `String`/`List`/`Map`" and return
    /// `Some(Type::Error)`. The constraint retry below relies on the `Option` arm's
    /// silent `None` to gate entry, so it neither double-checks args nor
    /// double-reports.
    ///
    /// Split out of [`Self::check_method_call`] so a `where` constraint can retry the
    /// dispatch on an `Option`'s inner type (see that call site).
    fn dispatch_builtin_method(&mut self, mc: &MethodCallExpr, obj_type: &Type) -> Option<Type> {
        let (base_name, bindings) = self.extract_type_name_and_bindings(obj_type);
        let type_name = base_name.unwrap_or_else(|| obj_type.to_string());
        match type_name.as_str() {
            "List" => {
                let elem_type = bindings
                    .get("T")
                    .cloned()
                    .or_else(|| {
                        if let Type::Generic(_, args) = obj_type {
                            args.first().cloned()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(Type::TypeVar("T".to_string()));
                self.check_list_method(mc, elem_type)
            }
            "String" => self.check_string_method(mc),
            "Map" => {
                let key_type = bindings
                    .get("K")
                    .cloned()
                    .unwrap_or(Type::TypeVar("K".to_string()));
                let val_type = bindings
                    .get("V")
                    .cloned()
                    .unwrap_or(Type::TypeVar("V".to_string()));
                self.check_map_method(mc, key_type, val_type)
            }
            "Option" => {
                let inner_type = bindings
                    .get("T")
                    .cloned()
                    .unwrap_or(Type::TypeVar("T".to_string()));
                self.check_option_method(mc, inner_type)
            }
            "Result" => {
                let ok_type = bindings
                    .get("T")
                    .cloned()
                    .unwrap_or(Type::TypeVar("T".to_string()));
                let err_type = bindings
                    .get("E")
                    .cloned()
                    .unwrap_or(Type::TypeVar("E".to_string()));
                self.check_result_method(mc, ok_type, err_type)
            }
            "ListBuilder" => {
                let elem_type = bindings
                    .get("T")
                    .cloned()
                    .or_else(|| {
                        if let Type::Generic(_, args) = obj_type {
                            args.first().cloned()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(Type::TypeVar("T".to_string()));
                self.check_list_builder_method(mc, elem_type)
            }
            "MapBuilder" => {
                let key_type = bindings
                    .get("K")
                    .cloned()
                    .unwrap_or(Type::TypeVar("K".to_string()));
                let val_type = bindings
                    .get("V")
                    .cloned()
                    .unwrap_or(Type::TypeVar("V".to_string()));
                self.check_map_builder_method(mc, key_type, val_type)
            }
            _ => None,
        }
    }

    /// Type-checks a method call (`obj.method(args)`), dispatching to built-in
    /// methods for `List`, `String`, `Map`, `Option`, and `Result`, or looking
    /// up user-defined methods and trait-bounded methods.
    pub(crate) fn check_method_call(&mut self, mc: &MethodCallExpr) -> Type {
        // Namespace calls (`json.encode(...)`, `user.createUser(...)`)
        // dispatch before evaluating the object: the parser models them as
        // a `MethodCallExpr` with `object: Ident("json"|"user")`, and
        // evaluating that object as a value would hit "undefined variable".
        // `check_namespace_call` skips when the name is shadowed by a local
        // binding, so a `let json = …` falls through to the value path.
        // It runs before the builtin-static carve-out below, so a module
        // aliased to a builtin type name (`import x.List` → `List`) would
        // win over `List.builder()`; that collision is purely theoretical
        // (a namespace bound to the exact name `List`/`Map`) and left as-is.
        if let Some(ty) = self.check_namespace_call(mc) {
            return ty;
        }
        // Recognize `List.builder()` /
        // `Map.builder()` before evaluating the object expression.
        // The parser models `Type.method(...)` as a method call with
        // `object: Ident("Type")`; evaluating the object first would
        // hit the "undefined variable `Type`" path. The carve-out
        // itself (in `check_builtin_static_method`) skips when the
        // receiver name shadows a local binding, so a user
        // `let List = some_value` then `List.builder()` falls through
        // to the normal value-receiver path instead of being silently
        // hijacked into the builtin.
        if let Some(ty) = self.check_builtin_static_method(mc) {
            return ty;
        }
        let obj_type = self.check_expr(&mc.object);
        if obj_type.is_error() {
            return Type::Error;
        }
        if obj_type == Type::Void {
            self.error(format!("cannot call method on {}", obj_type), mc.span);
            return Type::Error;
        }
        let (base_name, bindings) = self.extract_type_name_and_bindings(&obj_type);
        let type_name = base_name.unwrap_or_else(|| obj_type.to_string());

        // Dispatch to built-in type helpers.
        if let Some(ty) = self.dispatch_builtin_method(mc, &obj_type) {
            if !mc.type_args.is_empty() {
                self.error(
                    format!(
                        "built-in method `{}` does not take type arguments",
                        mc.method
                    ),
                    mc.span,
                );
            }
            return ty;
        }

        // Trait-object method dispatch: single-bound `dyn Trait` only.
        if let Type::Dyn(trait_name) = &obj_type {
            // Trait methods are not generic today, so an explicit turbofish has
            // nothing to bind — reject it rather than silently dropping it,
            // matching every other dispatch path above.
            if !mc.type_args.is_empty() {
                self.error(
                    format!("trait method `{}` does not take type arguments", mc.method),
                    mc.span,
                );
            }
            // Clone just the method signature so we don't hold a borrow of
            // `self.traits` across `check_method_args`. Sema rejects
            // `dyn UnknownTrait` upstream in `resolve_type_expr`, so the
            // trait must be present here; only the *method name* may be
            // wrong at this site.
            let Some(trait_info) = self.lookup_trait(trait_name) else {
                unreachable!(
                    "compiler bug: receiver typed `dyn {trait_name}` but trait is missing \
                     from sema metadata — `Checker::resolve_type_expr` must reject \
                     `dyn UnknownTrait` before checker reaches a method call on it"
                );
            };
            let method_sig = trait_info
                .methods
                .iter()
                .find(|m| m.name == mc.method)
                .map(|m| (m.params.clone(), m.return_type.clone()));
            return match method_sig {
                Some((params, ret)) => {
                    // Trait-object dispatch does not accept defaults — trait
                    // method defaults are a separate Phase-3 follow-up.
                    // Synthesize arg-style names for diagnostics — trait
                    // metadata does not carry parameter names today.
                    let param_names: Vec<String> =
                        (0..params.len()).map(|i| format!("arg{}", i + 1)).collect();
                    self.check_method_args(
                        mc,
                        &params,
                        &param_names,
                        &HashMap::new(),
                        &HashMap::new(),
                    );
                    ret
                }
                None => {
                    self.error(
                        format!("trait `{}` has no method `{}`", trait_name, mc.method),
                        mc.span,
                    );
                    Type::Error
                }
            };
        }

        // User-defined methods. Go through `lookup_methods` so the
        // receiver-type name is resolved through the current module's
        // scope — methods on a type declared in module `lib` are keyed
        // under `lib::User`, not the bare `User` the use-site wrote.
        if let Some(type_methods) = self.lookup_methods(&type_name).cloned()
            && let Some(method_info) = type_methods.get(&mc.method)
        {
            // Merge parent-type bindings (from the receiver) with bindings
            // inferred for the method's own type parameters (from the
            // argument types), then record the method's concrete type
            // args for IR monomorphization.
            let mut all_bindings = bindings.clone();
            if !method_info.type_params.is_empty() {
                if !mc.type_args.is_empty() {
                    // Explicit turbofish (`obj.method<Int>(x)`) overrides
                    // inference: resolve, validate arity, and record directly.
                    self.apply_explicit_method_type_args(
                        mc,
                        &method_info.type_params,
                        &type_name,
                        &mut all_bindings,
                    );
                } else {
                    // Pre-check arg types so inference has something to unify.
                    let arg_types: Vec<Type> = mc.args.iter().map(|a| self.check_expr(a)).collect();
                    let (method_bindings, errors) =
                        self.infer_type_args(&method_info.params, &arg_types);
                    for (k, v) in method_bindings.iter() {
                        all_bindings.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                    self.record_inferred_type_args(
                        &format!("{}.{}", type_name, mc.method),
                        &method_info.type_params,
                        &all_bindings,
                        &errors,
                        &arg_types,
                        mc.span,
                    );
                }
            } else if !mc.type_args.is_empty() {
                self.error(
                    format!("method `{}` does not take type arguments", mc.method),
                    mc.span,
                );
            }
            self.check_method_args(
                mc,
                &method_info.params,
                &method_info.param_names,
                &all_bindings,
                &method_info.default_param_exprs,
            );
            return Self::substitute(&method_info.return_type, &all_bindings);
        }
        // Check trait bounds for type variables
        if let Some(ty) = self.resolve_trait_bound_method(&obj_type, mc) {
            return ty;
        }
        // Last resort, inside a `where` constraint only: a String/List method
        // (`self.contains(...)`) on an `Option<T>` field operates on the inner value
        // — codegen nil-guards the access, exactly like the `self.length` /
        // numeric-comparison constraint forms. No path above resolved the method on
        // the `Option` itself (an `Option` method like `isSome` would have resolved
        // at the user-method path and returned already), so retry the built-in
        // dispatch on `T`. Placed here, after every `Option`-level path, so it never
        // shadows a real `Option` method.
        if self.in_constraint
            && let Type::Generic(n, args) = &obj_type
            && n == "Option"
            && args.len() == 1
        {
            let inner = args[0].clone();
            if let Some(ty) = self.dispatch_builtin_method(mc, &inner) {
                return ty;
            }
        }
        self.error(
            format!(
                "no method `{}` on type `{}`",
                mc.method,
                phoenix_common::module_path::bare_name(&type_name),
            ),
            mc.span,
        );
        Type::Error
    }

    /// Validates argument count and types for a method call against expected
    /// parameter types, applying generic substitutions from `bindings`.
    ///
    /// Slots in `[mc.args.len()..params.len())` are accepted when covered
    /// by `default_param_exprs`; callers without defaults (trait-object
    /// dispatch, trait-bound dispatch) pass an empty map.  Only the
    /// user-supplied positional args are type-checked here — default
    /// expressions are type-checked once at declaration time (see
    /// `check_impl`'s pass-1 in `checker.rs`).
    ///
    /// `param_names` must have the same length as `params`; it is used
    /// only for the "missing argument for parameter `x`" diagnostic.
    pub(crate) fn check_method_args(
        &mut self,
        mc: &MethodCallExpr,
        params: &[Type],
        param_names: &[String],
        bindings: &HashMap<String, Type>,
        default_param_exprs: &HashMap<usize, phoenix_parser::ast::Expr>,
    ) {
        debug_assert_eq!(
            params.len(),
            param_names.len(),
            "check_method_args: params/param_names length mismatch",
        );
        if mc.args.len() > params.len() {
            self.error(
                format!(
                    "method `{}` takes {} argument(s), got {}",
                    mc.method,
                    params.len(),
                    mc.args.len()
                ),
                mc.span,
            );
            return;
        }
        // Every uncovered slot in the user-visible tail must have a default.
        let missing: Vec<String> = (mc.args.len()..params.len())
            .filter(|i| !default_param_exprs.contains_key(i))
            .map(|i| param_names[i].clone())
            .collect();
        if !missing.is_empty() {
            self.error(
                format!(
                    "method `{}` missing argument(s): {}",
                    mc.method,
                    missing.join(", ")
                ),
                mc.span,
            );
            return;
        }
        for (i, arg) in mc.args.iter().enumerate() {
            let arg_type = self.check_expr(arg);
            let expected = Self::substitute(&params[i], bindings);
            if !arg_type.is_error()
                && !expected.is_error()
                && !self.types_compatible(&expected, &arg_type)
            {
                self.error(
                    format!(
                        "argument {} of `{}`: expected `{}` but got `{}`",
                        i + 1,
                        mc.method,
                        expected,
                        arg_type
                    ),
                    arg.span(),
                );
            }
            // Pin a constructor argument whose phantom type params are
            // unbound (`d.take(Ok(5))` where `take`'s parameter is
            // `Result<Int, String>`) to the concrete parameter type — the
            // method-call analogue of the free-function call-arg pin. See
            // `pin_inferred_type_to_annotation`.
            self.pin_inferred_type_to_annotation(arg, &expected);
        }
    }

    /// Looks up a method on a type variable via its trait bounds. Returns
    /// `Some(return_type)` if a matching trait method is found, `None` otherwise.
    fn resolve_trait_bound_method(&mut self, obj_type: &Type, mc: &MethodCallExpr) -> Option<Type> {
        let tv_name = match obj_type {
            Type::TypeVar(name) => name,
            _ => return None,
        };
        for (param_name, bound_traits) in &self.current_type_param_bounds.clone() {
            if param_name != tv_name {
                continue;
            }
            for bound_trait in bound_traits {
                let trait_info = match self.lookup_trait(bound_trait).cloned() {
                    Some(info) => info,
                    None => continue,
                };
                let trait_method = match trait_info.methods.iter().find(|m| m.name == mc.method) {
                    Some(m) => m,
                    None => continue,
                };
                // Trait methods are not generic today, so an explicit turbofish
                // has nothing to bind — reject it rather than silently dropping
                // it, matching every other dispatch path.
                if !mc.type_args.is_empty() {
                    self.error(
                        format!("trait method `{}` does not take type arguments", mc.method),
                        mc.span,
                    );
                }
                let empty_bindings = HashMap::new();
                // Trait-bound method dispatch ignores defaults for now —
                // defaults on trait methods require mono-time synthesis.
                let param_names: Vec<String> = (0..trait_method.params.len())
                    .map(|i| format!("arg{}", i + 1))
                    .collect();
                self.check_method_args(
                    mc,
                    &trait_method.params,
                    &param_names,
                    &empty_bindings,
                    &HashMap::new(),
                );
                return Some(trait_method.return_type.clone());
            }
        }
        None
    }

    /// Type-checks a struct constructor or enum variant constructor expression,
    /// validating field count, field types, and inferring generic type arguments.
    ///
    /// Cross-module visibility: constructing a struct declared in another
    /// (non-builtin) module emits a single batched diagnostic listing
    /// every private field — otherwise positional construction would let
    /// any module write a private field that read-side
    /// `check_field_access` correctly rejects, defeating encapsulation.
    pub(crate) fn check_struct_literal(&mut self, sl: &StructLiteralExpr) -> Type {
        // Check if it's a struct constructor
        if let Some(struct_info) = self.lookup_struct(&sl.name).cloned() {
            self.enforce_cross_module_construction_privacy(&struct_info, &sl.name, sl.span);
            if sl.args.len() != struct_info.fields.len() {
                self.error(
                    format!(
                        "struct `{}` has {} field(s), got {}",
                        sl.name,
                        struct_info.fields.len(),
                        sl.args.len()
                    ),
                    sl.span,
                );
            } else if !struct_info.type_params.is_empty() {
                let mut arg_types = Vec::new();
                for arg in &sl.args {
                    arg_types.push(self.check_expr(arg));
                }
                let field_types: Vec<Type> =
                    struct_info.fields.iter().map(|f| f.ty.clone()).collect();
                let (bindings, _) = self.infer_type_args(&field_types, &arg_types);
                for (i, arg) in sl.args.iter().enumerate() {
                    let expected = Self::substitute(&struct_info.fields[i].ty, &bindings);
                    // `types_compatible` so dyn-typed fields on a generic
                    // struct still get the concrete-to-dyn coercion.
                    if !expected.has_type_vars() && !self.types_compatible(&expected, &arg_types[i])
                    {
                        self.error(
                            format!(
                                "field `{}`: expected `{}` but got `{}`",
                                struct_info.fields[i].name, expected, arg_types[i]
                            ),
                            arg.span(),
                        );
                    }
                    // Pin a field-initializer constructor whose phantom type
                    // params are unbound (`Node(2, None)` where `next` is
                    // `Option<Node<Int>>`) to the concrete field type. See
                    // `pin_inferred_type_to_annotation`.
                    self.pin_inferred_type_to_annotation(arg, &expected);
                }
                let result_args: Vec<Type> = struct_info
                    .type_params
                    .iter()
                    .map(|p| bindings.get(p).cloned().unwrap_or(Type::TypeVar(p.clone())))
                    .collect();
                // Carry the qualified key so cross-module struct
                // construction produces a Type that matches the
                // qualified key the let-annotation resolves to.
                return Type::Generic(self.qualify_in_current(&sl.name), result_args);
            } else {
                for (i, arg) in sl.args.iter().enumerate() {
                    let arg_type = self.check_expr(arg);
                    let field = &struct_info.fields[i];
                    if !arg_type.is_error()
                        && !field.ty.is_error()
                        && !self.types_compatible(&field.ty, &arg_type)
                    {
                        self.error(
                            format!(
                                "field `{}`: expected `{}` but got `{}`",
                                field.name, field.ty, arg_type
                            ),
                            arg.span(),
                        );
                    }
                    let field_ty = field.ty.clone();
                    self.pin_inferred_type_to_annotation(arg, &field_ty);
                }
            }
            return Type::Named(self.qualify_in_current(&sl.name));
        }

        // Check if it's an enum variant constructor
        self.check_enum_variant_constructor(sl)
    }

    /// Type-checks an enum variant constructor expression, validating field
    /// count, field types, and inferring generic type arguments for the
    /// parent enum.
    ///
    /// Variant resolution goes through `lookup_visible_enum_variant` so
    /// only enums actually visible in the current module's scope are
    /// considered — a variant whose owning enum was never imported (or
    /// is private) does not resolve from here. The returned enum name
    /// is the *user-source* name (the alias, if any), not the qualified
    /// table key, so the `Type::Named(...)` / `Type::Generic(...)`
    /// produced here matches what users wrote in surrounding source.
    fn check_enum_variant_constructor(&mut self, sl: &StructLiteralExpr) -> Type {
        let variant_match = self.lookup_visible_enum_variant(&sl.name, sl.span);

        if let Some((enum_name, type_params, variant_types)) = variant_match {
            if sl.args.len() != variant_types.len() {
                self.error(
                    format!(
                        "variant `{}` takes {} field(s), got {}",
                        sl.name,
                        variant_types.len(),
                        sl.args.len()
                    ),
                    sl.span,
                );
            } else if !type_params.is_empty() {
                let mut arg_types = Vec::new();
                for arg in &sl.args {
                    arg_types.push(self.check_expr(arg));
                }
                let (bindings, _) = self.infer_type_args(&variant_types, &arg_types);
                for (i, arg) in sl.args.iter().enumerate() {
                    let expected = Self::substitute(&variant_types[i], &bindings);
                    // `types_compatible` so dyn-typed variant fields on a
                    // generic enum still get the concrete-to-dyn coercion.
                    if !expected.has_type_vars() && !self.types_compatible(&expected, &arg_types[i])
                    {
                        self.error(
                            format!(
                                "variant `{}` field {}: expected `{}` but got `{}`",
                                sl.name,
                                i + 1,
                                expected,
                                arg_types[i]
                            ),
                            arg.span(),
                        );
                    }
                    // Pin a nested constructor whose phantom type params are
                    // unbound (`Some(Ok(1))` where the payload is
                    // `Result<Int, String>`) to the concrete variant-field
                    // type. See `pin_inferred_type_to_annotation`.
                    self.pin_inferred_type_to_annotation(arg, &expected);
                }
                let result_args: Vec<Type> = type_params
                    .iter()
                    .map(|p| bindings.get(p).cloned().unwrap_or(Type::TypeVar(p.clone())))
                    .collect();
                // Qualify so this Type matches what
                // `resolve_type_expr` produces for the same enum at
                // a parameter / let annotation site.
                return Type::Generic(self.qualify_in_current(&enum_name), result_args);
            } else {
                for (i, arg) in sl.args.iter().enumerate() {
                    let arg_type = self.check_expr(arg);
                    if !arg_type.is_error()
                        && !variant_types[i].is_error()
                        && !self.types_compatible(&variant_types[i], &arg_type)
                    {
                        self.error(
                            format!(
                                "variant `{}` field {}: expected `{}` but got `{}`",
                                sl.name,
                                i + 1,
                                variant_types[i],
                                arg_type
                            ),
                            arg.span(),
                        );
                    }
                }
            }
            return Type::Named(self.qualify_in_current(&enum_name));
        }

        self.error(format!("undefined type or variant `{}`", sl.name), sl.span);
        Type::Error
    }

    /// Dispatch a namespace call `name.method(args)` where `name` is a
    /// namespace-import binding (`import models.user` / `import json`).
    /// Returns `None` when `mc.object` is not an `Ident`, the name is
    /// shadowed by a local value binding, or the name is not a namespace —
    /// callers then fall through to the normal receiver-value path.
    fn check_namespace_call(&mut self, mc: &MethodCallExpr) -> Option<Type> {
        let Expr::Ident(id) = &mc.object else {
            return None;
        };
        // A local binding of the same name shadows the namespace, exactly
        // like the builtin-static-method carve-out. The value-receiver
        // path then handles `name.method(...)` against that value's type.
        if self.scopes.lookup(&id.name).is_some() {
            return None;
        }
        match self.namespace_target(&id.name)? {
            NamespaceTarget::UserModule(path) => {
                Some(self.check_user_namespace_call(&id.name, &path, mc))
            }
            NamespaceTarget::Intrinsic(IntrinsicNs::Json) => {
                Some(self.check_json_namespace_call(mc))
            }
        }
    }

    /// Type-check `user.func(args)` against a user-module namespace: the
    /// callee must be a public function of the target module. Records the
    /// resolved qualified key for IR lowering and validates args through
    /// the shared function-call machinery (so generics, defaults, and
    /// arity diagnostics all match a normal call).
    fn check_user_namespace_call(
        &mut self,
        ns_name: &str,
        path: &ModulePath,
        mc: &MethodCallExpr,
    ) -> Type {
        // A namespaced free-function call infers its type args from the
        // arguments like any other call; an explicit turbofish has no
        // meaning here (the json intrinsic path defines its own type-arg
        // semantics and is handled separately).
        if !mc.type_args.is_empty() {
            self.error(
                format!(
                    "function `{}.{}` does not take type arguments",
                    ns_name, mc.method
                ),
                mc.span,
            );
        }
        let qualified = module_qualify(path, &mc.method);
        let Some(func_info) = self.functions.get(&qualified).cloned() else {
            self.error(
                format!("module `{path}` has no function `{}`", mc.method),
                mc.span,
            );
            // Surface errors inside the arguments too (matches the json
            // arm); a missing callee shouldn't swallow a bad arg.
            self.check_call_args(&mc.args);
            return Type::Error;
        };
        // Cross-module access honors visibility — only `public` functions
        // are reachable through a namespace (the 2.6 rule, applied here).
        if func_info.visibility != Visibility::Public {
            self.emit_private_access_diagnostic(
                format!("function `{}` is private to module `{path}`", mc.method),
                mc.span,
                func_info.definition_span,
                format!(
                    "mark `{}` as `public` in `{path}` to call it as `{ns_name}.{}`",
                    mc.method, mc.method,
                ),
            );
            self.check_call_args(&mc.args);
            return Type::Error;
        }
        // Hand off to IR lowering (I2): which `FuncId` this call targets.
        // (No `record_reference` here: regular method calls don't record
        // one either, and `MethodCallExpr` carries no span for the method
        // name to anchor a precise reference — deferred with I2.)
        self.namespace_call_targets.insert(mc.span, qualified);
        // Validate arguments via the shared call machinery. A namespace
        // call has no named args; we pass the arg slice directly rather
        // than synthesizing (and cloning into) a `CallExpr`.
        // Diagnostics show the source form (`user.add`, or the alias) — the
        // internal `::`-qualified key never leaks into user-facing messages.
        let display = format!("{ns_name}.{}", mc.method);
        self.check_call_with_info(&display, &func_info, &mc.args, &[], mc.span)
    }

    /// Dispatch a `json.<method>(args)` intrinsic call. The `json`
    /// namespace binds (Phase 4 imports), but its `encode`/`decode`
    /// members are synthesized by the JSON serialization work (Phase 4.6)
    /// and are not available yet.
    fn check_json_namespace_call(&mut self, mc: &MethodCallExpr) -> Type {
        match mc.method.as_str() {
            "encode" => self.check_json_encode(mc),
            "decode" => self.check_json_decode(mc),
            _ => {
                self.error(
                    format!(
                        "`json.{}` is not available yet — JSON serialization lands in Phase 4.6",
                        mc.method
                    ),
                    mc.span,
                );
                self.check_call_args(&mc.args);
                Type::Error
            }
        }
    }

    /// Type-check `json.decode<T>(text) -> Result<T, JsonError>`. `T` comes
    /// from an explicit turbofish (`json.decode<Int>(s)`); contextual
    /// inference (`let x: Int = json.decode(s)?`) lands in a later slice.
    /// Records `T` for the IR decoder-synthesis pass.
    fn check_json_decode(&mut self, mc: &MethodCallExpr) -> Type {
        let json_error = Type::Named("JsonError".to_string());
        // Argument: exactly one `String`. Track well-formedness so we only
        // record the decode type (which downstream IR lowering and the
        // interpreters key off, unconditionally indexing `args[0]`) when the
        // call is actually decodable.
        let mut args_ok = true;
        if mc.args.len() != 1 {
            self.error(
                format!("json.decode() takes 1 argument, got {}", mc.args.len()),
                mc.span,
            );
            self.check_call_args(&mc.args);
            args_ok = false;
        } else {
            let arg_ty = self.check_expr(&mc.args[0]);
            if !arg_ty.is_error() && arg_ty != Type::String {
                self.error(
                    format!("json.decode() expects a `String` argument, got `{arg_ty}`"),
                    mc.args[0].span(),
                );
                args_ok = false;
            }
        }
        // Target type `T` from the turbofish.
        let Some(target) = mc.type_args.first() else {
            self.error(
                "json.decode requires an explicit type argument, e.g. `json.decode<Int>(s)`"
                    .to_string(),
                mc.span,
            );
            return Type::Error;
        };
        if mc.type_args.len() > 1 {
            self.error(
                "json.decode takes a single type argument".to_string(),
                mc.span,
            );
        }
        let t = self.resolve_type_expr(target);
        if t.is_error() {
            return Type::Error;
        }
        if let Some(unsupported) = self.unsupported_json_decode_type(&t, &mut Vec::new()) {
            self.error(
                format!(
                    "`json.decode` does not support `{unsupported}` yet — \
                     supported today: Int, Float, Bool, String, `Option<T>`, `List<T>`, \
                     `Map<String, V>`, and non-generic structs and enums of supported \
                     types (non-`String`-key maps are a deferred follow-up)"
                ),
                mc.span,
            );
        } else if self.json_field_privacy_violation("json.decode", &t, mc.span) {
            // Decoding constructs every reachable struct; a foreign struct
            // with private fields is rejected (diagnostic already emitted)
            // and, like the unsupported case, records no decode site.
            // Anchored on the call span (not `args[0]` as on the encode
            // side): the violation travels with the type argument, while
            // encode's travels with the value argument.
        } else if args_ok {
            // Only a fully well-formed call (one `String` arg, supported `T`)
            // is handed to synthesis/lowering. A malformed call still returns
            // the `Result<T, JsonError>` type for error recovery, but records
            // no decode site — so the `args[0]`-indexing lowering path is
            // unreachable without an accompanying sema error.
            self.json_decode_types.insert(mc.span, t.clone());
        }
        Type::Generic("Result".to_string(), vec![t, json_error])
    }

    /// Returns the name of the first JSON-undecodable type reachable from
    /// `ty` (recursing through struct fields, enum variant fields, the inner
    /// type of `Option`/`List`, and a `Map`'s value type), or `None` when `ty`
    /// is decodable with today's surface: the scalars, `Option<T>`, `List<T>`,
    /// `Map<String, V>`, and non-generic structs and enums of decodable
    /// component types. Non-`String`-key maps and generic enums other than
    /// `Option` remain unsupported. `visiting` holds the names of the
    /// structs/enums currently being walked so a self-referential type can't
    /// recurse forever, mirroring the encode-side gate.
    fn unsupported_json_decode_type(
        &self,
        ty: &Type,
        visiting: &mut Vec<String>,
    ) -> Option<String> {
        match ty {
            Type::Int | Type::Float | Type::Bool | Type::String => None,
            // `Option<T>` decodes when `T` does (null → None, else Some(x)).
            Type::Generic(name, args) if name == OPTION_ENUM && args.len() == 1 => {
                self.unsupported_json_decode_type(&args[0], visiting)
            }
            // `List<T>` decodes from a JSON array when `T` does.
            Type::Generic(name, args) if name == "List" && args.len() == 1 => {
                self.unsupported_json_decode_type(&args[0], visiting)
            }
            // `Map<String, V>` decodes from a JSON object when `V` does. Maps
            // with non-`String` keys (which encode as `[k, v]` pairs) are a
            // deferred follow-up, matching the encode-side gate.
            Type::Generic(name, args)
                if name == "Map" && args.len() == 2 && args[0] == Type::String =>
            {
                self.unsupported_json_decode_type(&args[1], visiting)
            }
            Type::Named(name) => {
                // Resolved `Type::Named` names are already canonical — bare
                // for an entry-module type, module-qualified for an imported
                // one (see `resolve_type_expr`) — so the spelling here is
                // both the cycle-guard identity and the diagnostic spelling.
                if visiting.iter().any(|n| n == name) {
                    return None; // break cycles
                }
                // A non-generic struct: every field must be decodable.
                if let Some(info) = self.lookup_struct(name) {
                    if !info.type_params.is_empty() {
                        return Some(name.clone());
                    }
                    visiting.push(name.clone());
                    let r = info
                        .fields
                        .iter()
                        .find_map(|f| self.unsupported_json_decode_type(&f.ty, visiting));
                    visiting.pop();
                    return r;
                }
                // A non-generic enum: every variant's field types must decode.
                if let Some(info) = self.lookup_enum(name) {
                    if !info.type_params.is_empty() {
                        return Some(name.clone());
                    }
                    visiting.push(name.clone());
                    let r = info
                        .variants
                        .iter()
                        .flat_map(|(_, fts)| fts.iter())
                        .find_map(|t| self.unsupported_json_decode_type(t, visiting));
                    visiting.pop();
                    return r;
                }
                Some(name.clone())
            }
            other => Some(canonical_type_spelling(other)),
        }
    }

    /// Type-check `json.encode(value) -> String`. Records the argument's
    /// static type for the IR encode-synthesis pass and validates that the
    /// type is JSON-encodable with the currently-supported surface.
    fn check_json_encode(&mut self, mc: &MethodCallExpr) -> Type {
        if !mc.type_args.is_empty() {
            self.error(
                "`json.encode` does not take type arguments".to_string(),
                mc.span,
            );
        }
        if mc.args.len() != 1 {
            self.error(
                format!("json.encode() takes 1 argument, got {}", mc.args.len()),
                mc.span,
            );
            self.check_call_args(&mc.args);
            return Type::String;
        }
        let arg_type = self.check_expr(&mc.args[0]);
        if arg_type.is_error() {
            return Type::String;
        }
        if let Some(unsupported) = self.unsupported_json_encode_type(&arg_type, &mut Vec::new()) {
            self.error(
                format!(
                    "`json.encode` does not support `{unsupported}` yet — \
                     supported today: Int, Float, Bool, String, and structs of those \
                     (more types land in later Phase 4.6 slices)"
                ),
                mc.args[0].span(),
            );
            return Type::String;
        }
        // Encoding reads every reachable field; a foreign struct with
        // private fields is rejected (diagnostic already emitted) and
        // records no encode site.
        if self.json_field_privacy_violation("json.encode", &arg_type, mc.args[0].span()) {
            return Type::String;
        }
        self.json_encode_types.insert(mc.span, arg_type);
        Type::String
    }

    /// Returns the name of the first JSON-unencodable type reachable from
    /// `ty` (itself or, for a struct, a field type), or `None` when `ty` is
    /// fully encodable with today's surface. `visiting` guards against
    /// cyclic struct graphs.
    ///
    /// Phase 4.6 grows this surface in slices: today it is the scalars,
    /// `Option<T>`, `List<T>`, `Map<String, V>`, non-generic structs, and
    /// non-generic enums (each of supported component types); non-`String`-key
    /// maps and generic enums other than `Option` are added by later slices.
    fn unsupported_json_encode_type(
        &self,
        ty: &Type,
        visiting: &mut Vec<String>,
    ) -> Option<String> {
        match ty {
            Type::Int | Type::Float | Type::Bool | Type::String => None,
            // `Option<T>` is encodable when `T` is (None → null, Some(x) →
            // encode(x)). Other generic instantiations (`Result<…>`, generic
            // user enums) are deferred.
            Type::Generic(name, args) if name == OPTION_ENUM && args.len() == 1 => {
                self.unsupported_json_encode_type(&args[0], visiting)
            }
            // `List<T>` → array; encodable when `T` is.
            Type::Generic(name, args) if name == "List" && args.len() == 1 => {
                self.unsupported_json_encode_type(&args[0], visiting)
            }
            // `Map<String, V>` → JSON object, encodable when `V` is. Maps with
            // non-`String` keys (which serialize as `[k, v]` pairs) are
            // deferred to a follow-up slice.
            Type::Generic(name, args)
                if name == "Map" && args.len() == 2 && args[0] == Type::String =>
            {
                self.unsupported_json_encode_type(&args[1], visiting)
            }
            Type::Named(name) => {
                // Back-edge guard for cyclic struct/enum graphs: returning
                // `None` (encodable) terminates the walk on the cyclic type
                // itself. This is sound and load-bearing: a recursive type
                // closed through `Option<Self>` (e.g. `struct Node { next:
                // Option<Node> }`) IS encodable — the synthesized encoder is
                // itself recursive and terminates on the finite runtime value
                // (`Some(child)` / `None`). `List`/`Map` back-edges remain
                // gated as unsupported below. Comparing raw spellings is the
                // right identity: resolved `Type::Named` names are already
                // canonical (see `resolve_type_expr`), matching the
                // decode-side gate.
                if visiting.iter().any(|n| n == name) {
                    return None;
                }
                // A non-generic struct: every field must be encodable.
                // Diagnostics print the name as spelled in the resolved type
                // (the canonical name — bare for an entry-module type,
                // module-qualified for an imported one, so same-named types
                // from different modules stay distinguishable).
                if let Some(info) = self.lookup_struct(name) {
                    if !info.type_params.is_empty() {
                        return Some(name.clone());
                    }
                    let field_tys: Vec<Type> = info.fields.iter().map(|f| f.ty.clone()).collect();
                    return self.first_unsupported_component(name, &field_tys, visiting);
                }
                // A non-generic enum: every variant's field types must be
                // encodable. (Generic enums — including `Result` — are gated
                // by their `Type::Generic` shape above / below.)
                if let Some(info) = self.lookup_enum(name) {
                    if !info.type_params.is_empty() {
                        return Some(name.clone());
                    }
                    let variant_tys: Vec<Type> = info
                        .variants
                        .iter()
                        .flat_map(|(_, fts)| fts.iter().cloned())
                        .collect();
                    return self.first_unsupported_component(name, &variant_tys, visiting);
                }
                Some(name.clone())
            }
            other => Some(canonical_type_spelling(other)),
        }
    }

    /// Return the first JSON-unencodable type among `components` (a struct's
    /// field types or an enum's variant field types), with `name` pushed onto
    /// `visiting` for the duration of the walk so a cyclic back-edge through
    /// `components` terminates on `name` itself rather than recursing forever.
    fn first_unsupported_component(
        &self,
        name: &str,
        components: &[Type],
        visiting: &mut Vec<String>,
    ) -> Option<String> {
        visiting.push(name.to_string());
        let result = components
            .iter()
            .find_map(|ty| self.unsupported_json_encode_type(ty, visiting));
        visiting.pop();
        result
    }

    /// Type-check each argument expression for its side effects (surfacing
    /// nested diagnostics) while discarding the resulting types. Used by the
    /// namespace-call error paths, where the call itself already failed but
    /// errors *inside* the arguments should still be reported.
    fn check_call_args(&mut self, args: &[Expr]) {
        for arg in args {
            self.check_expr(arg);
        }
    }

    /// Recognize the static-method shape `TypeName.method(args)` for
    /// builtin types. Today only the builder
    /// constructors fire here (`List.builder()`, `Map.builder()`);
    /// future static methods on builtins land in the same dispatch.
    /// Returns `None` if the receiver isn't an `Ident` of a
    /// recognized builtin type name — callers fall through to the
    /// normal value-receiver path. (Phoenix's parser models
    /// `Type.method(...)` as a `MethodCallExpr` with
    /// `object: Ident("Type")`; this is *not* a `FieldAccess` call.)
    fn check_builtin_static_method(&mut self, mc: &MethodCallExpr) -> Option<Type> {
        let type_ident = match &mc.object {
            Expr::Ident(i) => i,
            _ => return None,
        };
        // Only intercept names registered in the shared builtin-static-
        // method table (`BUILTIN_STATIC_METHOD_TYPES`). The same table
        // gates the IR-lowering carve-out in `lower_method_call` so the
        // two sides can't diverge on which receivers get hijacked.
        if !crate::types::is_builtin_static_method_type(&type_ident.name) {
            return None;
        }
        // If the user shadowed the builtin name with a local
        // (`let List = some_value`), defer to that binding — the value
        // receiver path will report a sensible "no method on type X"
        // error if `.builder()` doesn't fit, instead of silently
        // routing to the builtin and surprising the user.
        if self.scopes.lookup(&type_ident.name).is_some() {
            return None;
        }
        let result = match (type_ident.name.as_str(), mc.method.as_str()) {
            ("List", "builder") => {
                if !mc.args.is_empty() {
                    self.error("List.builder() takes no arguments".to_string(), mc.span);
                }
                // Returns `ListBuilder<TypeVar("T"))`; the let-binding's
                // explicit annotation (e.g. `ListBuilder<Int>`) pins
                // `T` via the standard let-unification path.
                Some(crate::types::list_builder_of(Type::TypeVar(
                    "T".to_string(),
                )))
            }
            ("Map", "builder") => {
                if !mc.args.is_empty() {
                    self.error("Map.builder() takes no arguments".to_string(), mc.span);
                }
                Some(crate::types::map_builder_of(
                    Type::TypeVar("K".to_string()),
                    Type::TypeVar("V".to_string()),
                ))
            }
            _ => None,
        };
        // Builder constructors take their element types from the binding's
        // annotation, never from an explicit turbofish — reject it the same
        // way instance built-ins do, rather than silently dropping it. Only
        // fires once the call is recognized as a builtin static method, so an
        // unrecognized `Type.method<…>()` falls through without a stray error.
        if result.is_some() && !mc.type_args.is_empty() {
            self.error(
                format!(
                    "built-in `{}.{}` does not take type arguments",
                    type_ident.name, mc.method
                ),
                mc.span,
            );
        }
        result
    }

    /// Type-checks a function call expression, resolving the callee and
    /// validating argument count and types.  Handles named arguments and
    /// default parameter values.
    pub(crate) fn check_call(&mut self, call: &CallExpr) -> Type {
        if let Expr::Ident(ident) = &call.callee {
            // Built-in: print
            if ident.name == "print" {
                if !call.named_args.is_empty() {
                    self.error(
                        "built-in function `print` does not accept named arguments".to_string(),
                        call.span,
                    );
                }
                if call.args.len() != 1 {
                    self.error(
                        format!("print() takes 1 argument, got {}", call.args.len()),
                        call.span,
                    );
                } else {
                    self.check_expr(&call.args[0]);
                }
                return Type::Void;
            }
            // Built-in: toString
            if ident.name == "toString" {
                if !call.named_args.is_empty() {
                    self.error(
                        "built-in function `toString` does not accept named arguments".to_string(),
                        call.span,
                    );
                }
                if call.args.len() != 1 {
                    self.error(
                        format!("toString() takes 1 argument, got {}", call.args.len()),
                        call.span,
                    );
                } else {
                    self.check_expr(&call.args[0]);
                }
                return Type::String;
            }

            // User-defined function — go through the module scope so
            // imports + visibility (Phase B) are honored once they land.
            if let Some(func_info) = self.lookup_function(&ident.name).cloned() {
                self.record_reference(
                    ident.span,
                    crate::checker::SymbolKind::Function,
                    ident.name.clone(),
                );
                return self.check_call_with_info(
                    &ident.name,
                    &func_info,
                    &call.args,
                    &call.named_args,
                    call.span,
                );
            }

            // `extern js` host function (Phase 2.5). Externs live in a separate
            // table but share `FunctionInfo`, so call validation (arity, arg
            // types, return type) reuses `check_call_with_info` unchanged.
            if let Some(extern_info) = self.lookup_extern(&ident.name).cloned() {
                self.record_reference(
                    ident.span,
                    crate::checker::SymbolKind::Function,
                    ident.name.clone(),
                );
                return self.check_call_with_info(
                    &ident.name,
                    &extern_info,
                    &call.args,
                    &call.named_args,
                    call.span,
                );
            }

            // Check if it's a variable with a function type
            if let Some(info) = self.scopes.lookup(&ident.name).cloned() {
                return self.check_call_on_type(info.ty, call);
            }

            self.error(format!("undefined function `{}`", ident.name), ident.span);
            return Type::Error;
        }

        // Non-ident callee (e.g. lambda call) — check callee type
        let callee_type = self.check_expr(&call.callee);
        self.check_call_on_type(callee_type, call)
    }

    /// Validates a call against a known `FunctionInfo`, handling positional
    /// args, named args, and default parameter values.
    ///
    /// Takes the argument lists and call span as borrowed slices rather
    /// than a `&CallExpr` so callers without a real `CallExpr` node —
    /// namespace calls, which the parser models as a `MethodCallExpr` —
    /// can reuse it without synthesizing (and deep-cloning into) one. The
    /// callee expression is never consulted here.
    fn check_call_with_info(
        &mut self,
        func_name: &str,
        func_info: &FunctionInfo,
        args: &[Expr],
        named_args: &[(String, Expr)],
        call_span: Span,
    ) -> Type {
        let total_params = func_info.params.len();
        let positional_count = args.len();
        let named_count = named_args.len();

        self.validate_named_args(func_name, func_info, named_args, positional_count);

        // Positional args must not exceed total params
        if positional_count > total_params {
            self.error(
                format!(
                    "function `{}` takes {} argument(s), got {}",
                    func_name, total_params, positional_count
                ),
                call_span,
            );
            return func_info.return_type.clone();
        }

        // Check that all required (non-default) params are covered
        // Count how many params are covered: positional + named + default
        let mut covered = vec![false; total_params];
        for c in covered.iter_mut().take(positional_count.min(total_params)) {
            *c = true;
        }
        for (name, _) in named_args {
            if let Some(idx) = func_info.param_names.iter().position(|n| n == name) {
                covered[idx] = true;
            }
        }
        for &idx in func_info.default_param_exprs.keys() {
            covered[idx] = true; // defaults fill in uncovered params
        }
        let missing: Vec<String> = covered
            .iter()
            .enumerate()
            .filter(|(_, c)| !**c)
            .map(|(i, _)| func_info.param_names[i].clone())
            .collect();
        if !missing.is_empty() {
            self.error(
                format!(
                    "function `{}` missing argument(s): {}",
                    func_name,
                    missing.join(", ")
                ),
                call_span,
            );
            return func_info.return_type.clone();
        }

        // Also check total supplied doesn't exceed params
        // (positional + named should not provide more args than params)
        if positional_count + named_count > total_params {
            self.error(
                format!(
                    "function `{}` takes {} argument(s), got {} (positional) + {} (named)",
                    func_name, total_params, positional_count, named_count
                ),
                call_span,
            );
            return func_info.return_type.clone();
        }

        // Now type-check all provided arguments
        if !func_info.type_params.is_empty() {
            let (bindings, arg_types, errors) = self.infer_and_check_call_generics(
                func_name,
                func_info,
                args,
                named_args,
                call_span,
                positional_count,
            );
            self.record_inferred_type_args(
                func_name,
                &func_info.type_params,
                &bindings,
                &errors,
                &arg_types,
                call_span,
            );
            return Self::substitute(&func_info.return_type, &bindings);
        }

        // Non-generic: type-check positional args
        for (i, arg) in args.iter().enumerate() {
            let arg_type = self.check_expr(arg);
            if !arg_type.is_error()
                && !func_info.params[i].is_error()
                && !self.types_compatible(&func_info.params[i], &arg_type)
            {
                self.error(
                    format!(
                        "argument {} of `{}`: expected `{}` but got `{}`",
                        i + 1,
                        func_name,
                        func_info.params[i],
                        arg_type
                    ),
                    arg.span(),
                );
            }
            // Pin a constructor argument with unbound phantom type params
            // (`other(Ok(5))`) to the concrete parameter type. See
            // `pin_inferred_type_to_annotation`.
            self.pin_inferred_type_to_annotation(arg, &func_info.params[i]);
        }
        // Type-check named args
        for (name, expr) in named_args {
            if let Some(idx) = func_info.param_names.iter().position(|n| n == name) {
                let arg_type = self.check_expr(expr);
                if !arg_type.is_error()
                    && !func_info.params[idx].is_error()
                    && !self.types_compatible(&func_info.params[idx], &arg_type)
                {
                    self.error(
                        format!(
                            "named argument `{}` of `{}`: expected `{}` but got `{}`",
                            name, func_name, func_info.params[idx], arg_type
                        ),
                        expr.span(),
                    );
                }
                self.pin_inferred_type_to_annotation(expr, &func_info.params[idx]);
            }
        }

        func_info.return_type.clone()
    }

    /// Validates named arguments: checks for duplicates, unknown names, and
    /// overlap with positional arguments.
    fn validate_named_args(
        &mut self,
        func_name: &str,
        func_info: &FunctionInfo,
        named_args: &[(String, Expr)],
        positional_count: usize,
    ) {
        let mut named_set = HashSet::new();
        for (name, expr) in named_args {
            if !named_set.insert(name.clone()) {
                self.error(format!("duplicate named argument `{}`", name), expr.span());
            }
            if !func_info.param_names.contains(name) {
                self.error(
                    format!("function `{}` has no parameter named `{}`", func_name, name),
                    expr.span(),
                );
            }
        }
        for (name, expr) in named_args {
            if let Some(idx) = func_info.param_names.iter().position(|n| n == name)
                && idx < positional_count
            {
                self.error(
                    format!(
                        "parameter `{}` already provided as positional argument {}",
                        name,
                        idx + 1
                    ),
                    expr.span(),
                );
            }
        }
    }

    /// For a generic function call, builds the full argument type array,
    /// infers type variable bindings, type-checks each argument against its
    /// substituted parameter type, and validates trait bounds.
    ///
    /// Returns `(bindings, arg_types, errors)`:
    /// - `bindings` maps declared type parameters to their inferred concrete
    ///   types. May be incomplete if no argument constrains a parameter.
    /// - `arg_types` is the fully resolved `Vec<Type>` in declared parameter
    ///   order (defaults fill in uncovered positions). Used downstream by
    ///   [`record_inferred_type_args`](Self::record_inferred_type_args) to
    ///   suppress unresolved-param diagnostics when a cascade is already in
    ///   flight from `Type::Error` arguments.
    /// - `errors` are the binding-level failures ([`UnifyError::Conflict`]
    ///   and [`UnifyError::OccursCheck`]) discovered by
    ///   [`infer_type_args`](Self::infer_type_args).
    fn infer_and_check_call_generics(
        &mut self,
        func_name: &str,
        func_info: &FunctionInfo,
        args: &[Expr],
        named_args: &[(String, Expr)],
        call_span: Span,
        positional_count: usize,
    ) -> CallGenericsInference {
        let total_params = func_info.params.len();

        // Build the full arg_types array in param order
        let mut arg_types = vec![Type::Error; total_params];
        for (i, arg) in args.iter().enumerate() {
            arg_types[i] = self.check_expr(arg);
        }
        for (name, expr) in named_args {
            if let Some(idx) = func_info.param_names.iter().position(|n| n == name) {
                arg_types[idx] = self.check_expr(expr);
            }
        }
        // For params covered by defaults only, use the declared param type
        for (i, at) in arg_types.iter_mut().enumerate() {
            if *at == Type::Error
                && func_info.default_param_exprs.contains_key(&i)
                && i >= positional_count
                && !named_args
                    .iter()
                    .any(|(n, _)| *n == func_info.param_names[i])
            {
                *at = func_info.params[i].clone();
            }
        }
        let (bindings, errors) = self.infer_type_args(&func_info.params, &arg_types);

        // Type-check provided args against substituted param types.
        // `!has_type_vars()` gates the check until inference has resolved
        // every TypeVar in `expected` — leftover vars mean unification
        // didn't bind the parameter, and the unresolved-type-parameter
        // diagnostic in `record_inferred_type_args` is the better signal.
        // The compatibility check uses `types_compatible` (not `==`) so a
        // concrete arg flowing into a `dyn Trait` parameter coerces just
        // like in the non-generic call path.
        for (i, arg) in args.iter().enumerate() {
            let expected = Self::substitute(&func_info.params[i], &bindings);
            if !expected.has_type_vars() && !self.types_compatible(&expected, &arg_types[i]) {
                self.error(
                    format!(
                        "argument {} of `{}`: expected `{}` but got `{}`",
                        i + 1,
                        func_name,
                        expected,
                        arg_types[i]
                    ),
                    arg.span(),
                );
            }
            // Pin a constructor argument whose phantom type params are
            // unbound (`other(Ok(5))` where `other` takes
            // `Result<Int, String>`) to the concrete parameter type. See
            // `pin_inferred_type_to_annotation`.
            self.pin_inferred_type_to_annotation(arg, &expected);
        }
        for (name, expr) in named_args {
            if let Some(idx) = func_info.param_names.iter().position(|n| n == name) {
                let expected = Self::substitute(&func_info.params[idx], &bindings);
                if !expected.has_type_vars() && !self.types_compatible(&expected, &arg_types[idx]) {
                    self.error(
                        format!(
                            "named argument `{}` of `{}`: expected `{}` but got `{}`",
                            name, func_name, expected, arg_types[idx]
                        ),
                        expr.span(),
                    );
                }
                // Pin a phantom-param constructor passed as a *named* argument
                // to a generic function — the named-arg analogue of the
                // positional pin just above (the non-generic call path pins
                // both arms; this one must too). See
                // `pin_inferred_type_to_annotation`.
                self.pin_inferred_type_to_annotation(expr, &expected);
            }
        }

        // Check trait bounds. `has_trait_impl` resolves both the
        // concrete type-name and the trait-name through the current
        // module's scope so cross-module bounds (e.g. `T: lib::Display`
        // imported here) are honored.
        for (param_name, bound_traits) in &func_info.type_param_bounds {
            if let Some(concrete) = bindings.get(param_name) {
                let concrete_name = self.type_name_for_bounds(concrete);
                for bound_trait in bound_traits {
                    if !self.has_trait_impl(&concrete_name, bound_trait) {
                        self.error(
                            format!(
                                "type `{}` does not implement trait `{}`",
                                concrete_name, bound_trait
                            ),
                            call_span,
                        );
                    }
                }
            }
        }

        (bindings, arg_types, errors)
    }

    /// Apply explicit turbofish type arguments for a generic method call.
    ///
    /// Validates the count against the method's type parameters, resolves
    /// each `TypeExpr`, binds them, and inserts the ordered list into
    /// `call_type_args` — the same span-keyed channel the inferred path
    /// feeds, so IR monomorphizes the call identically. On an arity mismatch
    /// a diagnostic is emitted and nothing is recorded.
    fn apply_explicit_method_type_args(
        &mut self,
        mc: &MethodCallExpr,
        type_params: &[String],
        type_name: &str,
        bindings: &mut HashMap<String, Type>,
    ) {
        if mc.type_args.len() != type_params.len() {
            self.error(
                format!(
                    "method `{}.{}` expects {} type argument(s), got {}",
                    type_name,
                    mc.method,
                    type_params.len(),
                    mc.type_args.len()
                ),
                mc.span,
            );
            return;
        }
        let resolved: Vec<Type> = mc
            .type_args
            .iter()
            .map(|te| self.resolve_type_expr(te))
            .collect();
        // `insert` (overwrite), not `or_insert`: `type_params` are the
        // method's *own* binders, which lexically shadow any same-named
        // impl-level param already in `bindings` from the receiver. So the
        // explicit value must win for those keys. This is deliberately
        // *opposite* to the inferred path, which uses `or_insert` because
        // `infer_type_args` can hand back impl-level keys that the
        // receiver already pinned authoritatively — there the receiver
        // must win. The two never disagree in the common case (distinct
        // names); they only differ under a method/impl name collision, and
        // each path's choice is correct for its own key set.
        for (tp, ty) in type_params.iter().zip(resolved.iter()) {
            bindings.insert(tp.clone(), ty.clone());
        }
        if !resolved.iter().any(Type::is_error) {
            self.call_type_args.insert(mc.span, resolved);
        }
    }

    /// Finalize a generic call: emit diagnostics for any unification errors
    /// and for unresolved type parameters, then (if everything resolved
    /// cleanly) record the concrete type arguments in
    /// [`call_type_args`](Checker::call_type_args) keyed by `call_span` for
    /// IR monomorphization to consume.
    ///
    /// The contract is:
    /// - Conflicts and occurs-check failures always produce a diagnostic.
    /// - Unresolved type parameters produce a diagnostic **unless** some
    ///   argument already has `Type::Error` (suppresses cascades from
    ///   undefined identifiers and similar upstream errors).
    /// - Nothing is inserted into `call_type_args` if any diagnostic fires
    ///   or if any resolved type is `Type::Error`. Downstream IR lowering
    ///   relies on this invariant: entries always have fully-resolved
    ///   concrete types.
    pub(crate) fn record_inferred_type_args(
        &mut self,
        callee_name: &str,
        type_params: &[String],
        bindings: &HashMap<String, Type>,
        errors: &[(usize, UnifyError)],
        arg_types: &[Type],
        call_span: Span,
    ) {
        // Surface binding-level failures first.
        let mut had_hard_error = false;
        for (i, err) in errors {
            had_hard_error = true;
            match err {
                UnifyError::Conflict {
                    param,
                    existing,
                    incoming,
                } => {
                    self.error(
                        format!(
                            "argument {} of `{}`: conflicting bindings for type parameter `{}` (was `{}`, now `{}`)",
                            i + 1,
                            callee_name,
                            param,
                            existing,
                            incoming
                        ),
                        call_span,
                    );
                }
                UnifyError::OccursCheck { param, incoming } => {
                    self.error(
                        format!(
                            "argument {} of `{}`: cannot bind type parameter `{}` to `{}` (recursive type)",
                            i + 1,
                            callee_name,
                            param,
                            incoming
                        ),
                        call_span,
                    );
                }
                UnifyError::Mismatch => {}
            }
        }

        // Surface unresolved type parameters — but only if no argument is
        // already `Type::Error`, to avoid cascading diagnostics from
        // upstream failures (undefined identifiers, type errors in args).
        let has_error_arg = arg_types.iter().any(Type::is_error);
        let unresolved: Vec<&str> = type_params
            .iter()
            .filter(|tp| !bindings.contains_key(tp.as_str()))
            .map(String::as_str)
            .collect();
        if !unresolved.is_empty() && !has_error_arg {
            let names = unresolved
                .iter()
                .map(|n| format!("`{}`", n))
                .collect::<Vec<_>>()
                .join(", ");
            let (param_word, them) = if unresolved.len() == 1 {
                ("type parameter", "it")
            } else {
                ("type parameters", "them")
            };
            self.error(
                format!(
                    "cannot infer {} {} for call to `{}`; no argument constrains {}",
                    param_word, names, callee_name, them
                ),
                call_span,
            );
            return;
        }

        if had_hard_error {
            return;
        }

        let ordered: Option<Vec<Type>> = type_params
            .iter()
            .map(|tp| bindings.get(tp).cloned())
            .collect();
        if let Some(ordered) = ordered
            && !ordered.iter().any(Type::is_error)
        {
            self.call_type_args.insert(call_span, ordered);
        }
    }

    /// Checks a call expression against a callee that has a known type.
    ///
    /// Used when the callee is a variable with a `Type::Function` type or a
    /// lambda expression.  Validates that the number and types of arguments
    /// match the function's parameter list and returns the function's return
    /// type.  Named arguments and defaults are not supported for indirect calls.
    fn check_call_on_type(&mut self, callee_type: Type, call: &CallExpr) -> Type {
        if !call.named_args.is_empty() {
            self.error(
                "named arguments are not supported for indirect function calls".to_string(),
                call.span,
            );
        }
        if let Type::Function(ref param_types, ref return_type) = callee_type {
            if call.args.len() != param_types.len() {
                self.error(
                    format!(
                        "function takes {} argument(s), got {}",
                        param_types.len(),
                        call.args.len()
                    ),
                    call.span,
                );
            } else {
                for (i, arg) in call.args.iter().enumerate() {
                    let arg_type = self.check_expr(arg);
                    if !arg_type.is_error()
                        && !param_types[i].is_error()
                        && !self.types_compatible(&param_types[i], &arg_type)
                    {
                        self.error(
                            format!(
                                "argument {}: expected {} but got {}",
                                i + 1,
                                param_types[i],
                                arg_type
                            ),
                            arg.span(),
                        );
                    }
                }
            }
            return *return_type.clone();
        }
        if !callee_type.is_error() {
            self.error(
                format!("cannot call value of type {}", callee_type),
                call.span,
            );
        }
        Type::Error
    }
}

/// Spell `ty` for a JSON-gate diagnostic with *canonical* names throughout:
/// unlike `Display` (which strips module qualifiers for readability in
/// ordinary type errors), a rejected generic keeps its possibly-qualified
/// base name — `models::Wrapper<Int>`, not `Wrapper<Int>` — so same-named
/// types from different modules stay distinguishable, matching the gates'
/// `Type::Named` arms (which return the canonical name directly). Builtin
/// generics (`List`, `Map`, `Result`) are unaffected: their canonical names
/// are already bare.
fn canonical_type_spelling(ty: &Type) -> String {
    match ty {
        Type::Named(name) => name.clone(),
        Type::Generic(name, args) => {
            let args: Vec<String> = args.iter().map(canonical_type_spelling).collect();
            format!("{name}<{}>", args.join(", "))
        }
        other => other.to_string(),
    }
}
