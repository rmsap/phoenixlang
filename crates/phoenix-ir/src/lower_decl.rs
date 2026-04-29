//! Pass 1: Declaration registration.
//!
//! Registers struct/enum layouts, function stubs, and the lookup
//! tables.  Driven entirely by [`phoenix_sema::ResolvedModule`]'s
//! id-indexed tables (no AST walk) — see
//! [`phoenix_common::ids`] for the id contract and
//! [`phoenix_sema::resolved`] for the schema.  Synthesized callables
//! (closures, monomorphized specializations) are appended past
//! [`crate::module::IrModule::synthesized_start`] in later passes.

use crate::instruction::FuncId;
use crate::lower::{LoweringContext, lower_type};
use crate::module::{IrFunction, IrTraitInfo, IrTraitMethod};
use crate::types::IrType;
use phoenix_sema::checker::{FunctionInfo, MethodInfo};

/// `(type_name, type_params, fields)` triple staged by
/// `register_struct_layouts` before installing into `IrModule`.
type StructLayoutEntry = (String, Vec<String>, Vec<(String, IrType)>);

/// `(type_name, type_params, variants)` triple staged by
/// `register_enum_layouts` before installing into `IrModule`.
type EnumLayoutEntry = (String, Vec<String>, Vec<(String, Vec<IrType>)>);

impl<'a> LoweringContext<'a> {
    /// Pass 1: Register all declarations.
    ///
    /// Drives off [`ResolvedModule`]'s id-indexed tables, so the
    /// resulting `IrModule` automatically agrees with sema on every
    /// [`FuncId`] / [`StructId`] / [`EnumId`] / [`TraitId`] without
    /// needing to coordinate two AST walks.  No `Program` argument
    /// is needed because registration draws every name and id from
    /// the resolved tables — only `lower_function_bodies` (pass 2)
    /// walks the AST.
    pub(crate) fn register_declarations(&mut self) {
        // Mirror sema's object-safe traits into IR-level metadata so
        // verifier / codegen / interpreter can answer "slot count" and
        // "method signature" without sampling impls or reaching back
        // into sema.  See `IrModule::traits`.
        self.register_traits();

        // Layouts (struct / enum) carry no FuncIds but are read by
        // body lowering and monomorphization; register both up front.
        self.register_struct_layouts();
        self.register_enum_layouts();

        // Pre-size `module.functions` with `FuncId(u32::MAX)`
        // sentinels, then overwrite each slot at its matching FuncId.
        //
        // Why a sentinel here instead of `Vec<Option<IrFunction>>`
        // (the shape `build_from_checker` uses on the sema side)?
        // Every downstream pass — body lowering, monomorphization,
        // verifier, codegen — reads `module.functions[id.index()]`
        // hot.  `Option<IrFunction>` would force an `unwrap` at
        // every call site for a contract that's already enforced
        // here at the boundary; a sentinel keeps reads zero-cost
        // and the `debug_assert!` at the end of this function
        // catches any unfilled slot before pass 2 runs.
        let n_functions = self.check.functions.len();
        let n_user_methods = self.check.user_methods.len();
        let total_callables = n_functions + n_user_methods;
        self.module.user_method_offset = self.check.user_method_offset;
        self.module.synthesized_start = total_callables as u32;
        debug_assert_eq!(
            self.module.user_method_offset as usize, n_functions,
            "ResolvedModule.user_method_offset disagrees with functions.len(); \
             build_from_checker invariants violated"
        );
        self.module.functions.reserve(total_callables);
        self.module.functions.resize_with(total_callables, || {
            crate::module::FunctionSlot::Concrete(IrFunction::new(
                FuncId(u32::MAX),
                String::new(),
                Vec::new(),
                Vec::new(),
                IrType::Void,
                None,
            ))
        });

        // Free functions in FuncId order (matches sema pre-pass A).
        for (name, func_id, info) in self.check.functions_with_names() {
            self.register_function_from_info(name, func_id, info);
        }

        // User methods in FuncId order (matches sema pre-pass B).
        for ((type_name, method_name), func_id, info) in self.check.user_methods_with_names() {
            self.register_method_from_info(type_name, method_name, func_id, info);
        }

        // Orphan-method slots: sema's `user_methods` Vec includes
        // entries for methods whose parent decl was rejected
        // (within-module duplicates, coherence-violating impls). Their
        // FuncIds were pre-allocated and the slots are filled in
        // `ResolvedModule::user_methods` (so sema's invariants hold),
        // but they have no entry in `method_index` and are therefore
        // skipped by `user_methods_with_names()` above. They still
        // occupy `IrModule::functions` slots that the sentinel-fill
        // sized for; install a no-op placeholder at each remaining
        // slot so the post-registration invariant ("every FuncId slot
        // is filled") holds. The placeholder is unreachable: nothing
        // in `function_index` / `method_index` resolves to it, and
        // body lowering doesn't iterate by id. The driver
        // short-circuits on diagnostics before IR runs in the normal
        // path, so this only matters when IR is invoked on a sema
        // result that produced orphans (e.g. tooling that bypasses
        // the diagnostic gate).
        let mut placeholder_fills: u32 = 0;
        for (i, slot) in self.module.functions.iter_mut().enumerate() {
            if slot.func().id.0 == u32::MAX {
                *slot = crate::module::FunctionSlot::Concrete(IrFunction::new(
                    FuncId(i as u32),
                    String::new(),
                    Vec::new(),
                    Vec::new(),
                    IrType::Void,
                    None,
                ));
                placeholder_fills += 1;
            }
        }

        // Sanity-check: every slot we sized for must have been
        // filled.  An unfilled `FuncId(u32::MAX)` slot would indicate a
        // pre-allocated id with no matching ResolvedModule entry *and*
        // no orphan-fill placeholder — either a sema bug or a
        // divergence between
        // `pending_function_ids.len() + pending_user_method_ids.len()`
        // and the populated `functions` / `user_methods` Vec lengths.
        debug_assert!(
            self.module
                .functions
                .iter()
                .all(|s| s.func().id.0 != u32::MAX),
            "register_declarations left an unfilled FuncId slot — \
             ResolvedModule's functions/user_methods Vec did not cover \
             every pre-allocated id"
        );
        // The number of placeholder fills must equal the orphan-method
        // count sema reported. A mismatch means either the orphan-fill
        // pass missed a slot (named registration left an unexpected
        // hole) or it filled extras (named registration didn't cover
        // every named slot). Both are sema-IR shape divergences.
        debug_assert_eq!(
            placeholder_fills, self.check.orphan_method_count,
            "orphan-fill placeholder count ({placeholder_fills}) disagrees with \
             ResolvedModule.orphan_method_count ({})",
            self.check.orphan_method_count
        );
    }

    /// Populate `IrModule::traits` from sema's object-safe trait
    /// declarations. Skips non-object-safe traits (they cannot appear in
    /// `DynRef` positions, so no IR consumer needs their signatures).
    fn register_traits(&mut self) {
        for (name, info) in self.check.traits_with_names() {
            if info.object_safety_error.is_some() {
                continue;
            }
            let methods: Vec<IrTraitMethod> = info
                .methods
                .iter()
                .map(|m| IrTraitMethod {
                    name: m.name.clone(),
                    param_types: m.params.iter().map(|t| lower_type(t, self.check)).collect(),
                    return_type: lower_type(&m.return_type, self.check),
                })
                .collect();
            self.module
                .traits
                .insert(name.to_string(), IrTraitInfo { methods });
        }
    }

    /// Register every user-declared struct's IR layout.
    ///
    /// Reads `(name, type_params, fields)` from
    /// [`ResolvedModule::structs`](phoenix_sema::ResolvedModule::structs)
    /// in [`StructId`] order and installs each entry into
    /// [`IrModule::struct_layouts`].  Generic structs additionally
    /// register their type parameters in
    /// [`IrModule::struct_type_params`] for `monomorphize::struct_mono`
    /// to consume.  This pass carries no [`FuncId`]s — it just makes
    /// layouts available before body lowering / monomorphization.
    fn register_struct_layouts(&mut self) {
        // Collect to a Vec because `self.check` borrow conflicts with
        // mutating `self.module` inside the loop.  Length is small
        // (number of structs in the program), so the Vec is cheap.
        let entries: Vec<StructLayoutEntry> = self
            .check
            .structs_with_names()
            .map(|(name, _id, info)| {
                let fields: Vec<(String, IrType)> = info
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), lower_type(&f.ty, self.check)))
                    .collect();
                (name.to_string(), info.type_params.clone(), fields)
            })
            .collect();
        for (name, type_params, fields) in entries {
            self.module.struct_layouts.insert(name.clone(), fields);
            if !type_params.is_empty() {
                self.module.struct_type_params.insert(name, type_params);
            }
        }
    }

    /// Register every enum's IR layout.
    ///
    /// Reads `(name, type_params, variants)` from
    /// [`ResolvedModule::enums`](phoenix_sema::ResolvedModule::enums)
    /// in [`EnumId`] order and installs each entry into
    /// [`IrModule::enum_layouts`].  Generic enums additionally
    /// register their type parameters in
    /// [`IrModule::enum_type_params`].  Built-in `Option` and
    /// `Result` are skipped here — they're installed by
    /// `register_builtin_enum_layouts` because their generic
    /// payload-type handling differs from user-declared enums.
    fn register_enum_layouts(&mut self) {
        let check = self.check;
        let entries: Vec<EnumLayoutEntry> = check
            .enums_with_names()
            .filter(|(name, _, _)| {
                *name != crate::types::OPTION_ENUM && *name != crate::types::RESULT_ENUM
            })
            .map(|(name, _id, info)| {
                let variants: Vec<(String, Vec<IrType>)> = info
                    .variants
                    .iter()
                    .map(|(vname, fields)| {
                        let ir_fields: Vec<IrType> =
                            fields.iter().map(|t| lower_type(t, check)).collect();
                        (vname.clone(), ir_fields)
                    })
                    .collect();
                (name.to_string(), info.type_params.clone(), variants)
            })
            .collect();
        for (name, type_params, variants) in entries {
            self.module.enum_layouts.insert(name.clone(), variants);
            if !type_params.is_empty() {
                self.module.enum_type_params.insert(name, type_params);
            }
        }
    }

    /// Register a free function stub from sema's [`FunctionInfo`].
    ///
    /// Adopts `func_id` verbatim from sema (which pre-allocated it in
    /// pre-pass A) so the stub lands at the same slot in
    /// [`IrModule::functions`] that
    /// [`ResolvedModule::functions`](phoenix_sema::ResolvedModule::functions)
    /// already occupies — this is the load-bearing sema↔IR id
    /// contract.  Lowers param and return types via [`lower_type`]
    /// using the resolved-module's type tables; body lowering
    /// (`lower_function_bodies`) attaches IR ops to the stub later.
    fn register_function_from_info(&mut self, name: &str, func_id: FuncId, info: &FunctionInfo) {
        let param_types: Vec<IrType> = info
            .params
            .iter()
            .map(|t| lower_type(t, self.check))
            .collect();
        let param_names = info.param_names.clone();
        let return_type = lower_type(&info.return_type, self.check);

        let mut func = IrFunction::new(
            func_id,
            name.to_string(),
            param_types,
            param_names,
            return_type,
            Some(info.definition_span),
        );
        func.type_param_names = info.type_params.clone();

        let slot = if info.type_params.is_empty() {
            crate::module::FunctionSlot::Concrete(func)
        } else {
            crate::module::FunctionSlot::Template(func)
        };
        self.module.functions[func_id.index()] = slot;
        self.module.function_index.insert(name.to_string(), func_id);
    }

    /// Register a user-method stub from sema's [`MethodInfo`].
    ///
    /// Adopts `func_id` verbatim from sema (pre-allocated in pre-pass
    /// B), prepends the receiver type as the first parameter when
    /// `info.has_self`, and installs the stub into both
    /// [`IrModule::functions`] (at the matching `FuncId` slot) and
    /// [`IrModule::method_index`] (keyed by `(type_name, method_name)`).
    /// Methods on generic structs are placed in a
    /// [`crate::module::FunctionSlot::Template`] slot so monomorphization
    /// specializes them per concrete `StructId` substitution.
    fn register_method_from_info(
        &mut self,
        type_name: &str,
        method_name: &str,
        func_id: FuncId,
        info: &MethodInfo,
    ) {
        let mangled_name = format!("{type_name}.{method_name}");
        let mut param_types: Vec<IrType> = info
            .params
            .iter()
            .map(|t| lower_type(t, self.check))
            .collect();
        let mut param_names: Vec<String> = info.param_names.clone();

        // Single struct lookup — reused for both the self-type
        // construction and the slot-variant decision below.
        let struct_info = self.check.struct_info_by_name(type_name);

        if info.has_self {
            // Self type for the method template.  For generic structs,
            // the self-type args are the declared type-parameter names
            // lifted into `IrType::TypeVar`; struct-monomorphization
            // substitutes them with concrete types and clones the body
            // into a specialized `method_index` entry keyed by the
            // mangled struct name.  See `monomorphize::struct_mono`.
            //
            // The enum branch still carries the legacy gate: methods
            // on generic enums remain unsupported (separate
            // `known-issues` entry, Phase 4 target).  Touching the
            // gate requires the same struct-mono-style reification
            // for enum layouts, which is out of scope for this PR.
            let self_type = if let Some(s) = struct_info {
                let args: Vec<IrType> = s
                    .type_params
                    .iter()
                    .map(|name| IrType::TypeVar(name.clone()))
                    .collect();
                IrType::StructRef(type_name.to_string(), args)
            } else {
                let is_generic = self
                    .check
                    .enum_info_by_name(type_name)
                    .is_some_and(|info| !info.type_params.is_empty());
                if is_generic {
                    panic!(
                        "method on generic enum `{type_name}` reached IR lowering — \
                         `Checker::register_impl` (phoenix-sema/src/check_register.rs) \
                         is expected to reject this until monomorphization threads \
                         enum type_params into the self-type's args. See \
                         docs/known-issues.md: \"Methods on generic enums are gated off\"."
                    );
                }
                IrType::EnumRef(type_name.to_string(), Vec::new())
            };
            param_types.insert(0, self_type);
            param_names.insert(0, "self".to_string());
        }

        let return_type = lower_type(&info.return_type, self.check);

        let mut func = IrFunction::new(
            func_id,
            mangled_name,
            param_types,
            param_names,
            return_type,
            Some(info.definition_span),
        );
        func.type_param_names = info.type_params.clone();
        // A method on a generic struct is a template even when it has
        // no method-level type params — the struct's type params flow
        // into the body via the `self` parameter's `StructRef` args,
        // so the body contains `IrType::TypeVar` and cannot reach
        // Cranelift until struct-monomorphization specializes it.
        let parent_is_generic_struct = struct_info.is_some_and(|s| !s.type_params.is_empty());
        let is_template = !info.type_params.is_empty() || parent_is_generic_struct;

        let slot = if is_template {
            crate::module::FunctionSlot::Template(func)
        } else {
            crate::module::FunctionSlot::Concrete(func)
        };
        self.module.functions[func_id.index()] = slot;
        self.module
            .method_index
            .insert((type_name.to_string(), method_name.to_string()), func_id);
    }
}
